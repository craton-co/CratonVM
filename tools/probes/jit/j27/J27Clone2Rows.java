// j27 wave 4 (lane CLONE2): an instance clone() INSIDE A SPLICED CALLEE, bound
// to the virtual clone thin helper from the callee's own receiver profile
// (CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND, default ON; under
// CRATONVM_JIT_CLONE_VIRTUAL_THIN). Each callee is `final` and small, so the
// hot caller splices it with no receiver guard: every receiver, including a
// class that first appears after the caller compiled, runs the spliced body and
// its bound clone() site, and the helper must screen each one itself.
// Pages: docs/feature-designs/native-dispatch-architecture.md
//        docs/known-issues/jit/javac-zipfs-typecheck-sites-and-instance-clone-leave-compiled-code-20261027.md
//
// Expected HotSpot 25 output (facts only):
//   fact splice-copy true
//   fact splice-sum 25493856
//   fact midrun-inherit-copy true
//   fact midrun-inherit-class J27Clone2Rows$Point2
//   fact midrun-override-tagged 50000
//   fact midrun-override-class J27Clone2Rows$Point3
//   fact midrun-original-untouched true
//   fact after-copy true
//   fact cnse-warm-thrown 0
//   fact cnse-thrown 50000
//   fact cnse-message J27Clone2Rows$Maybe
//   fact cnse-after-thrown 0
// -Dj27.timing=true adds `time <row>-ns-per-op <n>` lines (not compared).
//
// Host check (not part of the facts): with CRATONVM_DBG_JITC=1 the caller's
// compile prints `clone-virtual-bind door=splice-seed class=J27Clone2Rows$Point
// ... Served` and `... class=J27Clone2Rows$Maybe ... Served` (the warm phase's
// dominant classes are Point and Yes); with
// CRATONVM_JIT_CLONE_SPLICED_INSTANCE_BIND=0 no `door=splice-seed` line, and the
// facts are unchanged.

public class J27Clone2Rows {
    static final int N = 200_000;
    static final int M = 50_000;

    static boolean timing = Boolean.getBoolean("j27.timing");

    /** Cloneable, inherits Object.clone. */
    static class Point implements Cloneable {
        int x;
        int y;
        int tag;

        Point(int x, int y) {
            this.x = x;
            this.y = y;
        }

        /** Small and final: spliced unguarded; `invokevirtual Point.clone`. */
        final Point copy() throws CloneNotSupportedException {
            return (Point) clone();
        }
    }

    /** First instantiated mid-run; inherits Object.clone (served). */
    static class Point2 extends Point {
        long z;
        String s;

        Point2(int x) {
            super(x, -x);
            this.z = (long) x << 32;
            this.s = "q" + (x & 3);
        }
    }

    /** First instantiated mid-run; overrides clone (the helper must decline). */
    static class Point3 extends Point {
        Point3(int x) {
            super(x, 0);
        }

        @Override
        protected Object clone() throws CloneNotSupportedException {
            Point3 c = (Point3) super.clone();
            c.tag = 7;
            return c;
        }
    }

    /** Not Cloneable; its subclass Yes is. */
    static class Maybe {
        int v;

        /** Small and final: spliced unguarded; `invokevirtual Maybe.clone`. */
        final Object tryCopy() throws CloneNotSupportedException {
            return clone();
        }
    }

    static class Yes extends Maybe implements Cloneable {
    }

    static String cnseMessage = "";

    public static void main(String[] args) throws Exception {
        spliceRows();
        cnseRows();
    }

    static void time(String row, long t0, long ops) {
        if (timing) {
            System.out.println("time " + row + "-ns-per-op " + (System.nanoTime() - t0) / ops);
        }
    }

    /** The hot caller: `p.copy()` spliced, its clone() site the one under test. */
    static long copyLoop(Point p, int n, boolean[] ok) throws CloneNotSupportedException {
        long sum = 0;
        Class<?> k = p.getClass();
        for (int i = 0; i < n; i++) {
            p.x = i;
            Point c = p.copy();
            ok[0] &= c != p && c.getClass() == k && c.x == i && c.y == p.y;
            sum += c.x & 0xff;
            sum += (long) c.tag << 32;
        }
        return sum;
    }

    static void spliceRows() throws CloneNotSupportedException {
        boolean[] ok = {true};
        long t0 = System.nanoTime();
        long sum = copyLoop(new Point(0, 4), N, ok);
        time("splice", t0, N);
        System.out.println("fact splice-copy " + ok[0]);
        // sum over i of (i & 0xff): 781 full cycles of 0..255 (32640 each) plus 0..63.
        System.out.println("fact splice-sum " + sum);

        // A served subclass that did not exist when the caller compiled.
        Point2 p2 = new Point2(5);
        boolean[] inheritOk = {true};
        copyLoop(p2, M, inheritOk);
        Point c2 = p2.copy();
        Point2 q2 = (Point2) c2;
        inheritOk[0] &= q2 != p2 && q2.z == p2.z && q2.s == p2.s && q2.y == p2.y && q2.tag == 0;
        System.out.println("fact midrun-inherit-copy " + inheritOk[0]);
        System.out.println("fact midrun-inherit-class " + c2.getClass().getName());

        // An overriding subclass that did not exist when the caller compiled:
        // every copy runs Point3.clone (tag 7), the original keeps tag 0.
        Point3 p3 = new Point3(9);
        boolean[] overrideOk = {true};
        long tagged = copyLoop(p3, M, overrideOk) >>> 32;
        Point c3 = p3.copy();
        overrideOk[0] &= p3.tag == 0;
        System.out.println("fact midrun-override-tagged " + (tagged / 7));
        System.out.println("fact midrun-override-class " + c3.getClass().getName());
        System.out.println("fact midrun-original-untouched " + overrideOk[0]);

        // The first class again: still copied right.
        boolean[] afterOk = {true};
        copyLoop(new Point(0, 11), M, afterOk);
        System.out.println("fact after-copy " + afterOk[0]);
    }

    /** The hot caller: `m.tryCopy()` spliced; CloneNotSupportedException thrown inside it. */
    static int cnseLoop(Maybe m, int n) {
        int thrown = 0;
        for (int i = 0; i < n; i++) {
            m.v = i;
            try {
                Object c = m.tryCopy();
                if (c == m || c.getClass() != m.getClass() || ((Maybe) c).v != i) {
                    throw new AssertionError("clone at " + i);
                }
            } catch (CloneNotSupportedException e) {
                thrown++;
                cnseMessage = e.getMessage();
            }
        }
        return thrown;
    }

    static void cnseRows() {
        long t0 = System.nanoTime();
        int warm = cnseLoop(new Yes(), N);
        time("cnse-warm", t0, N);
        int thrown = cnseLoop(new Maybe(), M);
        int after = cnseLoop(new Yes(), M);
        System.out.println("fact cnse-warm-thrown " + warm);
        System.out.println("fact cnse-thrown " + thrown);
        System.out.println("fact cnse-message " + cnseMessage);
        System.out.println("fact cnse-after-thrown " + after);
    }
}
