// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * JIT j27 wave 2, lane EASPLICE: facts for the escape-analysis / scalar
 * replacement / lock elision paths and the IR splice (inliner) paths the
 * wave-2 review read (docs/known-issues/jit/j27-ea-splice-review-rows-20261028.md).
 *
 * Every stdout line must equal HotSpot 25 byte for byte, in --real-jdk and
 * --compatible. Each row warms its method with the common-path arguments, so
 * the optimizing tier compiles it with its allocations scalar-replaced, its
 * locks elided and its callees spliced, and then calls it once on the path
 * that observes the optimisation: a deopt with a live replaced object, an
 * identity read, a class loaded after the compile, a first <clinit>, an
 * exception thrown inside a spliced body. The expected lines are in the page.
 *
 * No timing lines.
 */
public final class J27EaSpliceRows {
    static final int WARM = 30_000;

    static Object KEEP;
    static Object KEEP2;
    static long SINK;
    static final StringBuilder LOG = new StringBuilder();

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static String thrown(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    /** The innermost `depth` frames as `Class.method:line`, joined by `>`. */
    static String frames(Throwable t, int depth) {
        StackTraceElement[] st = t.getStackTrace();
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < depth && i < st.length; i++) {
            if (i > 0) {
                sb.append('>');
            }
            sb.append(st[i].getClassName()).append('.').append(st[i].getMethodName())
                    .append(':').append(st[i].getLineNumber());
        }
        return sb.toString();
    }

    // ── (a) escape analysis, scalar replacement, lock elision ────────────

    static final class P {
        int x;
        int y;

        P(int x, int y) {
            this.x = x;
            this.y = y;
        }
    }

    static final class Holder {
        Object o;
    }

    /** identityHashCode of a fresh object: stable, and equal to Object.hashCode. */
    static boolean idHashStable(int i) {
        P p = new P(i, i + 1);
        int h1 = System.identityHashCode(p);
        int h2 = p.hashCode();
        return h1 == h2 && h1 == System.identityHashCode(p) && p.x + 1 == p.y;
    }

    /** synchronized on a fresh object, waited on once (1 ms) on the last call. */
    static int waitLocal(int i) throws InterruptedException {
        Object o = new Object();
        synchronized (o) {
            if (i == WARM) {
                o.wait(1);
            }
        }
        return i;
    }

    /** notify under the lock is fine; notify outside it must throw. */
    static String notifyUnlocked(int i) {
        Object o = new Object();
        synchronized (o) {
            o.notify();
        }
        if (i == WARM) {
            try {
                o.notify();
                return "no-throw";
            } catch (IllegalMonitorStateException e) {
                return thrown(e);
            }
        }
        return "warm";
    }

    /** Thread.holdsLock inside and outside a lock on a fresh object. */
    static int holdsLocal() {
        Object o = new Object();
        int r = 0;
        synchronized (o) {
            if (Thread.holdsLock(o)) {
                r += 1;
            }
        }
        if (!Thread.holdsLock(o)) {
            r += 10;
        }
        return r;
    }

    /** Reference compares a fold must leave alone, and ones it may fold. */
    static int refCmp(Object param, int n) {
        Object a = new Object();
        int c = 0;
        if (a == param) {
            c += 1;
        }
        Object prev = null;
        for (int k = 0; k < n; k++) {
            Object cur = (k < 2) ? a : new Object();
            if (cur == prev) {
                c += 10;
            }
            prev = cur;
        }
        Object[] box = {a};
        if (box[0] == a) {
            c += 100;
        }
        Holder h = new Holder();
        h.o = a;
        if (h.o == a) {
            c += 1000;
        }
        return c;
    }

    /** A small array with variable indices (never scalar-replaceable). */
    static int varIndex(int i) {
        int[] arr = new int[4];
        arr[i & 3] = i;
        arr[(i + 1) & 3] = 2 * i;
        return arr[(i + 1) & 3] + arr[i & 3] + arr[(i + 2) & 3];
    }

    /** A constant index past a constant length: the throw must survive. */
    static int constOob(boolean go) {
        int[] a = new int[2];
        a[0] = 1;
        if (go) {
            return a[3];
        }
        return a[0];
    }

    /** A negative constant length on the rare path. */
    static int negLen(boolean go) {
        if (go) {
            int[] b = new int[-1];
            return b.length;
        }
        int[] c = new int[3];
        return c.length;
    }

    /** A covariant store into a fresh array. */
    static int covariant(boolean go) {
        Object[] a = new String[2];
        if (go) {
            a[0] = Integer.valueOf(1);
        } else {
            a[0] = "s";
        }
        return a.length;
    }

    static final class Carrier extends RuntimeException {
        final P p;

        Carrier(P p) {
            super(null, null, false, false);
            this.p = p;
        }
    }

    static int thrower(int i) {
        P p = new P(i, 2 * i);
        if (i == WARM) {
            throw new Carrier(p);
        }
        return p.x + p.y;
    }

    /** A fresh object escaping through a thrown exception. */
    static int viaThrow(int i) {
        try {
            return thrower(i);
        } catch (Carrier c) {
            KEEP = c;
            return -1;
        }
    }

    /** A replaced object materialised at a rare-path escape (deopt). */
    static int deoptPoint(int i, boolean rare) {
        P p = new P(i, i * 3);
        if (rare) {
            KEEP = p;
            KEEP2 = p;
            return -1;
        }
        return p.x + p.y;
    }

    /** A locked fresh object reaching a rare-path escape while locked. */
    static int lockedDeopt(int i, boolean rare) {
        P p = new P(i, 1);
        synchronized (p) {
            p.y += i;
            if (rare) {
                KEEP = p;
                return Thread.holdsLock(p) ? 1 : 0;
            }
        }
        return p.y;
    }

    // ── (b) splice planner: receivers, class loading, recursion ──────────

    abstract static class Shape {
        abstract int area();
    }

    static final class Sq extends Shape {
        final int s;

        Sq(int s) {
            this.s = s;
        }

        @Override
        int area() {
            return s * s;
        }
    }

    /** Never referenced before the warm-up ends: loaded after the compile. */
    static final class Tri extends Shape {
        final int s;

        Tri(int s) {
            this.s = s;
        }

        @Override
        int area() {
            return s * s / 2 + 7;
        }
    }

    static int area(Shape s) {
        return s.area();
    }

    static Shape makeTri(int s) {
        return new Tri(s);
    }

    static class Base {
        int v() {
            return 1;
        }
    }

    /** A late override of a method the warm-up saw with one implementation. */
    static final class Late extends Base {
        @Override
        int v() {
            return 20;
        }
    }

    static int callV(Base b) {
        return b.v();
    }

    static Base makeLate() {
        return new Late();
    }

    interface Op {
        int ap(int x);
    }

    static final class Inc implements Op {
        public int ap(int x) {
            return x + 1;
        }
    }

    static final class Dbl implements Op {
        public int ap(int x) {
            return x * 2;
        }
    }

    static final class Neg implements Op {
        public int ap(int x) {
            return -x;
        }
    }

    static int apply(Op op, int x) {
        return op.ap(x);
    }

    static int fib(int n) {
        return n < 2 ? n : fib(n - 1) + fib(n - 2);
    }

    static boolean isEven(int n) {
        return n == 0 ? true : isOdd(n - 1);
    }

    static boolean isOdd(int n) {
        return n == 0 ? false : isEven(n - 1);
    }

    // ── (b) exception handlers of spliced bodies ─────────────────────────

    static int inner(int[] a, int i) {
        return a[i];
    }

    static int outerCatch(int[] a, int i) {
        try {
            return inner(a, i) + 1;
        } catch (ArrayIndexOutOfBoundsException e) {
            return -100;
        }
    }

    static int calleeHandler(int d) {
        try {
            return 100 / d;
        } catch (ArithmeticException e) {
            throw new IllegalStateException("wrapped " + e.getMessage());
        }
    }

    static String callerOfHandler(int d) {
        try {
            return String.valueOf(calleeHandler(d));
        } catch (IllegalStateException e) {
            return thrown(e);
        }
    }

    static int FINALLY_RUNS;

    static int calleeFinally(int[] a, int i) {
        try {
            return a[i];
        } finally {
            FINALLY_RUNS++;
        }
    }

    static int callerOfFinally(int[] a, int i) {
        try {
            return calleeFinally(a, i);
        } catch (ArrayIndexOutOfBoundsException e) {
            return -7;
        }
    }

    // ── (b) synchronized spliced methods ─────────────────────────────────

    static final class Counter {
        int n;

        synchronized int incOrThrow(int k) {
            n++;
            if (k < 0) {
                throw new IllegalArgumentException("neg " + k);
            }
            return n;
        }

        synchronized boolean held() {
            return Thread.holdsLock(this);
        }

        static int sn;

        static synchronized int sInc(int k) {
            sn++;
            if (k < 0) {
                throw new IllegalArgumentException("sneg " + k);
            }
            return sn;
        }
    }

    static int callInc(Counter c, int k) {
        try {
            return c.incOrThrow(k);
        } catch (IllegalArgumentException e) {
            return Thread.holdsLock(c) ? -2 : -1;
        }
    }

    static int callSInc(int k) {
        try {
            return Counter.sInc(k);
        } catch (IllegalArgumentException e) {
            return Thread.holdsLock(Counter.class) ? -2 : -1;
        }
    }

    // ── (b) static initialisation barriers ───────────────────────────────

    static final class Lazy {
        static final int V;

        static {
            LOG.append("Lazy.<clinit>;");
            V = 42;
        }

        static int get() {
            LOG.append("get;");
            return V;
        }
    }

    static int maybeLazy(boolean go) {
        if (go) {
            return Lazy.get();
        }
        return 1;
    }

    static final class LazyField {
        static int F = init();

        static int init() {
            LOG.append("LazyField.<clinit>;");
            return 5;
        }
    }

    static int maybeField(boolean go) {
        if (go) {
            return LazyField.F;
        }
        return 2;
    }

    /** A class whose first `new` is a non-escaping (scalar-replaceable) one. */
    static final class LazyNew {
        static {
            LOG.append("LazyNew.<clinit>;");
        }

        int x = 9;
    }

    static int maybeNew(boolean go) {
        if (go) {
            LazyNew o = new LazyNew();
            return o.x;
        }
        return 3;
    }

    static final class Bad {
        static int V = fail();

        static int fail() {
            throw new RuntimeException("boom");
        }

        static int get() {
            return V;
        }
    }

    static String maybeBad(boolean go) {
        if (go) {
            try {
                return String.valueOf(Bad.get());
            } catch (Throwable t) {
                Throwable c = t.getCause();
                return thrown(t) + " cause=" + (c == null ? "null" : thrown(c));
            }
        }
        return "warm";
    }

    // ── (b) stack traces of exceptions thrown inside spliced bodies ──────

    static final class Node2 {
        Node2 next;
        int v;

        int nextV() {
            return next.v;
        }
    }

    static int readNext(Node2 n) {
        return n.nextV() + 1;
    }

    static int deepC(int k) {
        if (k < 0) {
            throw new IllegalStateException("deep " + k);
        }
        return k + 1;
    }

    static int deepB(int k) {
        return deepC(k) * 2;
    }

    static int deepA(int k) {
        return deepB(k) + 3;
    }

    static String whoCalls() {
        return new Throwable().getStackTrace()[1].getMethodName();
    }

    static String callerName() {
        return whoCalls();
    }

    public static void main(String[] args) throws Exception {
        rowsEa();
        rowsSplice();
        rowsHandlers();
        rowsInit();
        rowsTraces();
    }

    static void rowsEa() throws Exception {
        int bad = 0;
        for (int i = 0; i <= WARM; i++) {
            if (!idHashStable(i)) {
                bad++;
            }
        }
        fact("ea-idhash-unstable", bad);

        long s = 0;
        for (int i = 0; i <= WARM; i++) {
            s += waitLocal(i);
        }
        fact("ea-wait-local", s);

        String n = "";
        for (int i = 0; i <= WARM; i++) {
            n = notifyUnlocked(i);
        }
        fact("ea-notify-unlocked", n);

        int h = 0;
        for (int i = 0; i <= WARM; i++) {
            h = holdsLocal();
        }
        fact("ea-holds-local", h);

        Object other = new Object();
        int rc = 0;
        for (int i = 0; i <= WARM; i++) {
            rc = refCmp(other, 4);
        }
        fact("ea-ref-cmp", rc);
        fact("ea-ref-cmp-null", refCmp(null, 4));

        long vs = 0;
        for (int i = 0; i <= WARM; i++) {
            vs += varIndex(i);
        }
        fact("ea-var-index", vs);

        for (int i = 0; i < WARM; i++) {
            SINK += constOob(false);
        }
        try {
            fact("ea-const-oob", constOob(true));
        } catch (ArrayIndexOutOfBoundsException e) {
            fact("ea-const-oob", thrown(e));
        }

        for (int i = 0; i < WARM; i++) {
            SINK += negLen(false);
        }
        try {
            fact("ea-neg-len", negLen(true));
        } catch (NegativeArraySizeException e) {
            fact("ea-neg-len", thrown(e));
        }

        for (int i = 0; i < WARM; i++) {
            SINK += covariant(false);
        }
        try {
            fact("ea-covariant", covariant(true));
        } catch (ArrayStoreException e) {
            fact("ea-covariant", thrown(e));
        }

        for (int i = 0; i < WARM; i++) {
            SINK += viaThrow(i);
        }
        int vt = viaThrow(WARM);
        P cp = ((Carrier) KEEP).p;
        fact("ea-throw-escape", vt + "," + cp.x + "," + cp.y);

        for (int i = 0; i < WARM; i++) {
            SINK += deoptPoint(i, false);
        }
        int dr = deoptPoint(WARM, true);
        P dp = (P) KEEP;
        fact("ea-deopt-materialise", dr + "," + dp.x + "," + dp.y + "," + (KEEP == KEEP2));

        for (int i = 0; i < WARM; i++) {
            SINK += lockedDeopt(i, false);
        }
        int lr = lockedDeopt(WARM, true);
        P lp = (P) KEEP;
        fact("ea-locked-deopt", lr + "," + lp.y + "," + Thread.holdsLock(lp));
    }

    static void rowsSplice() {
        Shape sq = new Sq(3);
        long t = 0;
        for (int i = 0; i < WARM; i++) {
            t += area(sq);
        }
        fact("splice-cha-warm", t);
        Shape tri = makeTri(4);
        fact("splice-cha-late-subclass", area(tri) + "," + area(sq));

        Base b = new Base();
        long v = 0;
        for (int i = 0; i < WARM; i++) {
            v += callV(b);
        }
        fact("splice-late-override-warm", v);
        fact("splice-late-override", callV(makeLate()) + "," + callV(b));

        Op inc = new Inc();
        Op dbl = new Dbl();
        long o = 0;
        for (int i = 0; i < WARM; i++) {
            o += apply((i & 1) == 0 ? inc : dbl, i & 1023);
        }
        fact("splice-bimorphic-warm", o);
        fact("splice-bimorphic-third", apply(new Neg(), 5) + "," + apply(inc, 5) + "," + apply(dbl, 5));

        long f = 0;
        for (int i = 0; i < 2_000; i++) {
            f += fib(15);
        }
        fact("splice-recursive", f + "," + fib(25));
        boolean e = false;
        for (int i = 0; i < WARM; i++) {
            e ^= isEven(i & 63);
        }
        fact("splice-mutual-recursive", e + "," + isEven(1001) + "," + isOdd(777));
    }

    static void rowsHandlers() {
        int[] a = {1, 2, 3};
        long s = 0;
        for (int i = 0; i < WARM; i++) {
            s += outerCatch(a, i % 3);
        }
        fact("handler-caller-catch", s + "," + outerCatch(a, 5));

        String last = "";
        for (int i = 0; i < WARM; i++) {
            last = callerOfHandler((i & 7) + 1);
        }
        fact("handler-callee-rethrow", last + "|" + callerOfHandler(0));

        FINALLY_RUNS = 0;
        long fs = 0;
        for (int i = 0; i < WARM; i++) {
            fs += callerOfFinally(a, i % 3);
        }
        int fr = callerOfFinally(a, 9);
        fact("handler-callee-finally", fs + "," + fr + "," + FINALLY_RUNS);

        Counter c = new Counter();
        long cs = 0;
        for (int i = 0; i < WARM; i++) {
            cs += callInc(c, i);
        }
        int ci = callInc(c, -1);
        fact("sync-splice-throw", ci + "," + c.n + "," + Thread.holdsLock(c) + "," + c.held());

        long ss = 0;
        for (int i = 0; i < WARM; i++) {
            ss += callSInc(i);
        }
        int si = callSInc(-1);
        fact("sync-splice-static-throw", si + "," + Counter.sn + "," + Thread.holdsLock(Counter.class));
    }

    static void rowsInit() {
        for (int i = 0; i < WARM; i++) {
            SINK += maybeLazy(false);
            SINK += maybeField(false);
            SINK += maybeNew(false);
            maybeBad(false);
        }
        LOG.append("before;");
        int l = maybeLazy(true);
        LOG.append("mid;");
        int lf = maybeField(true);
        LOG.append("mid2;");
        int ln = maybeNew(true);
        LOG.append("after;");
        fact("clinit-order", LOG);
        fact("clinit-values", l + "," + lf + "," + ln + "," + maybeLazy(true) + "," + maybeNew(true));
        fact("clinit-fail-first", maybeBad(true));
        fact("clinit-fail-second", maybeBad(true));
    }

    static void rowsTraces() {
        Node2 n = new Node2();
        n.next = new Node2();
        n.next.v = 4;
        long s = 0;
        for (int i = 0; i < WARM; i++) {
            s += readNext(n);
        }
        fact("trace-npe-warm", s);
        Node2 broken = new Node2();
        try {
            fact("trace-npe", readNext(broken));
        } catch (NullPointerException e) {
            fact("trace-npe", thrown(e));
            fact("trace-npe-frames", frames(e, 3));
        }

        long d = 0;
        for (int i = 0; i < WARM; i++) {
            d += deepA(i & 255);
        }
        fact("trace-deep-warm", d);
        try {
            fact("trace-deep", deepA(-1));
        } catch (IllegalStateException e) {
            fact("trace-deep", thrown(e));
            fact("trace-deep-frames", frames(e, 4));
        }

        String who = "";
        for (int i = 0; i < WARM; i++) {
            who = callerName();
        }
        fact("trace-capture-in-body", who);
    }
}
