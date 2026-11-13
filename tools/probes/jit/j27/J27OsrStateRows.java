// j27 wave 5 (lane OSRHUNT): on-stack-replacement STATE rows, both tiers.
// Page: docs/feature-designs/jit-osr-exit-and-recompile.md
//
// Every loop runs in ONE call of a cold method (300,000 back edges, past both
// HotSpot's and CratonVM's OSR thresholds), so it is entered through the OSR
// door. The rows check what an OSR entry has to transfer and what an OSR exit
// has to give back:
//   * long / double two-slot locals and params, a carried negative int, a
//     float -0.0, a long loop counter past 2^32;
//   * a slot javac reuses (a ref, then a long; a long's high half, then ints)
//     and a slot that is TOP at the loop header (ref on one path, float on the
//     other, entered from both);
//   * locks held across the OSR entry: a synchronized method's receiver, a
//     static synchronized method's class, nested synchronized blocks -- with
//     Thread.holdsLock inside and after, and another thread that must be able
//     to take every lock once the method has left (normally and by a throw);
//   * an exception thrown out of an OSR'd loop to an outer handler, a handler
//     inside the loop (one throw per 1000 iterations), and an inner loop whose
//     header is re-entered after each exception exit;
//   * a CHA dependency broken inside the OSR body (a subclass first loaded
//     mid-loop);
//   * objects referenced only from the transferred locals across System.gc();
//   * OSR in <clinit> (own statics read and written while the class is being
//     initialized, and a throw out of the loop: EIIE then NCDFE);
//   * OSR in a constructor BEFORE super(..) (JDK 25 flexible constructor
//     bodies, JEP 513 -- compile with javac 25, no --release below 25), a
//     throw from there, and a field loop after super().
// Not expressible from Java source (javac never leaves a value on the operand
// stack across a back edge, and never makes a loop header a handler target):
// those two shapes need a jasm class; see the page.
//
// Every answer is plain Java arithmetic or a fixed message, so the expected
// lines are HotSpot's on any JVM:
//   javac --release 25 -d <dir> J27OsrStateRows.java
//   java -cp <dir> J27OsrStateRows > hs.txt
//   cratonvm --cp <dir> J27OsrStateRows | diff - hs.txt   (both modes, and --nojit)
//
// Expected HotSpot 25 output:
//   fact osr-long-sum 1200044999850000
//   fact osr-long-counter 5000300000 300000
//   fact osr-double-sum 75000.5
//   fact osr-float 300000.0 -Infinity
//   fact osr-two-slot-params 4398050711107
//   fact osr-neg-int-carried -2100000 -314998950000 -90000000000
//   fact osr-slot-ref-then-long 44999850000
//   fact osr-slot-highhalf-then-ints 600000
//   fact osr-top-local 600000 600000 600000 600000
//   fact osr-top-local-int-gc 600000 600000 600000 600000
//   fact osr-sync-method 44999850000 in=T after=T caller=F free
//   fact osr-sync-method-throw sync 299998 held=F hits=299998 free
//   fact osr-static-sync 44999850000 TF free
//   fact osr-nested-sync 44999850000 TT TF FF free
//   fact osr-nested-sync-throw stop 299995 FF hits=299995 free
//   fact osr-exc-outer at 299997 counter=299997
//   fact osr-exc-in-loop caught=300 s=44954700300
//   fact osr-reentry-after-exit 300003000006
//   fact osr-cha-break-mid-loop 450000 450000
//   fact osr-gc-only-locals 42 anchor7 true 9600000
//   fact osr-clinit 44999850000 600000
//   fact osr-clinit-throw EIIE boom 299990 | NCDFE Could not initialize class J27OsrStateRows$InitBad
//   fact osr-ctor-pre-super 44999850000 300000
//   fact osr-ctor-pre-super-throw pre 299999
public class J27OsrStateRows {
    static final int N = 300_000;
    static long sink;
    static int hits;
    static int counter;

    static String tf(boolean b) {
        return b ? "T" : "F";
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    // ── two-slot and primitive state ─────────────────────────────────

    static long longSum(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += i + 4_000_000_000L;
        return s;
    }

    static String longCounter(int n) {
        long end = 5_000_000_000L + n;
        long j;
        int cnt = 0;
        for (j = 5_000_000_000L; j < end; j++) cnt++;
        return j + " " + cnt;
    }

    static double doubleSum(int n) {
        double d = 0.5;
        for (int i = 0; i < n; i++) d += 0.25;
        return d;
    }

    static String floatRow(int n) {
        float fz = -0.0f;
        float f = 0f;
        for (int i = 0; i < n; i++) f += 1.0f;
        return f + " " + (1f / fz);
    }

    static double twoSlotParams(long base, double scale, int n, float f) {
        long acc = base;
        double d = scale;
        for (int i = 0; i < n; i++) {
            acc += 3;
            d += 0.5;
        }
        return acc + d + f;
    }

    static String negCarry(int n, int neg) {
        int m = -n;
        long acc = 0, acc2 = 0, acc3 = 0;
        for (int i = 0; i < n; i++) {
            acc += neg;
            acc2 += (long) neg * i;
            acc3 += m;
        }
        return acc + " " + acc2 + " " + acc3;
    }

    // ── slot reuse and TOP ───────────────────────────────────────────

    static long slotRefThenLong(int n) {
        {
            String s = "abc";
            sink += s.length();
        }
        long t = 0; // reuses the String's slot
        for (int i = 0; i < n; i++) t += i;
        return t;
    }

    static long slotHighHalfThenInts(int n) {
        {
            long a = 7L;
            sink += a;
        }
        int k = 3; // the long's low slot
        int m = -1; // the long's high half
        long t = 0;
        for (int i = 0; i < n; i++) t += k + m;
        return t;
    }

    static int topLocal(int n, boolean flag) {
        int r;
        if (flag) {
            Object o = new Object();
            r = o.hashCode() & 0;
        } else {
            float f = 1.5f;
            r = (int) f - 1;
        }
        // `o` / `f` share one slot, TOP from here on.
        for (int i = 0; i < n; i++) r += 2;
        return r;
    }

    /** TOP slot shared by a ref and an INT (both GPR-homed), collected in-loop. */
    static int topLocalIntGc(int n, boolean flag) {
        int r;
        if (flag) {
            Object o = new int[] {9};
            r = o.hashCode() & 0;
        } else {
            int q = 0x1234_5678; // a word that is no heap address
            r = q - 0x1234_5678;
        }
        for (int i = 0; i < n; i++) {
            r += 2;
            if ((i & 65535) == 1) System.gc();
        }
        return r;
    }

    // ── locks across the OSR entry ───────────────────────────────────

    /** Can another thread take all of `locks` (nested, in order) now? */
    static String freeFor(Object... locks) {
        boolean[] ok = new boolean[1];
        Thread t = new Thread(() -> lockAll(locks, 0, ok));
        t.start();
        try {
            t.join(10_000);
        } catch (InterruptedException e) {
            return "interrupted";
        }
        return ok[0] ? "free" : "stuck";
    }

    static void lockAll(Object[] locks, int i, boolean[] ok) {
        if (i == locks.length) {
            ok[0] = true;
            return;
        }
        synchronized (locks[i]) {
            lockAll(locks, i + 1, ok);
        }
    }

    static final class Holder {
        boolean in, after;

        synchronized long syncLoop(int n) {
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += i;
                if (i == n - 1) in = Thread.holdsLock(this);
            }
            after = Thread.holdsLock(this);
            return s;
        }

        synchronized void syncThrow(int n) {
            for (int i = 0; i < n; i++) {
                if (i == n - 2) throw new IllegalStateException("sync " + i);
                hits++;
            }
        }
    }

    static boolean staticIn;

    static synchronized long staticSyncLoop(int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            s += i;
            if (i == n - 1) staticIn = Thread.holdsLock(J27OsrStateRows.class);
        }
        return s;
    }

    static String nested(Object a, Object b, int n) {
        boolean inA = false, inB = false, afA, afB;
        long s = 0;
        synchronized (a) {
            synchronized (b) {
                for (int i = 0; i < n; i++) {
                    s += i;
                    if (i == n - 1) {
                        inA = Thread.holdsLock(a);
                        inB = Thread.holdsLock(b);
                    }
                }
            }
            afA = Thread.holdsLock(a);
            afB = Thread.holdsLock(b);
        }
        return s + " " + tf(inA) + tf(inB) + " " + tf(afA) + tf(afB) + " "
                + tf(Thread.holdsLock(a)) + tf(Thread.holdsLock(b));
    }

    static String nestedThrow(Object a, Object b, int n) {
        String msg;
        hits = 0;
        try {
            synchronized (a) {
                synchronized (b) {
                    for (int i = 0; i < n; i++) {
                        if (i == n - 5) throw new IllegalStateException("stop " + i);
                        hits++;
                    }
                }
            }
            msg = "none";
        } catch (IllegalStateException e) {
            msg = e.getMessage();
        }
        return msg + " " + tf(Thread.holdsLock(a)) + tf(Thread.holdsLock(b)) + " hits=" + hits;
    }

    // ── exceptions ───────────────────────────────────────────────────

    static void thrower(int n) {
        for (int i = 0; i < n; i++) {
            if (i == n - 3) throw new IllegalStateException("at " + i);
            counter++;
        }
    }

    static String excInLoop(int n) {
        int caught = 0;
        long s = 0;
        for (int i = 0; i < n; i++) {
            try {
                if (i % 1000 == 999) throw new RuntimeException("r" + i);
                s += i;
            } catch (RuntimeException e) {
                caught++;
            }
        }
        return "caught=" + caught + " s=" + s;
    }

    static long reentry(int n) {
        long s = 0;
        int exits = 0;
        for (int r = 0; r < 6; r++) {
            try {
                for (int i = 0; ; i++) {
                    s += i;
                    if (i == n) throw new IllegalStateException();
                }
            } catch (IllegalStateException e) {
                exits++;
            }
        }
        return s * 10 + exits;
    }

    // ── CHA broken inside the OSR body ───────────────────────────────

    abstract static class Shape {
        abstract int area();
    }

    static final class Sq extends Shape {
        int area() {
            return 1;
        }
    }

    static final class Tri extends Shape { // first loaded mid-loop
        int area() {
            return 2;
        }
    }

    static long chaMid(int n) {
        Shape s = new Sq();
        long t = 0;
        for (int i = 0; i < n; i++) {
            if (i == n / 2) s = new Tri();
            t += s.area();
        }
        return t;
    }

    // ── GC with objects held only by the transferred locals ──────────

    static String gcOnlyLocals(int n) {
        int[] keep = new int[] {42};
        Object anchor = new StringBuilder("anchor").append(7);
        java.lang.ref.WeakReference<Object> w = new java.lang.ref.WeakReference<>(anchor);
        long junk = 0;
        for (int i = 0; i < n; i++) {
            byte[] b = new byte[32];
            junk += b.length;
            if ((i & 65535) == 0) System.gc();
        }
        return keep[0] + " " + anchor + " " + (w.get() == anchor) + " " + junk;
    }

    // ── <clinit> ─────────────────────────────────────────────────────

    static final class InitSum {
        static final long V;
        static long W;

        static {
            long s = 0;
            for (int i = 0; i < 300_000; i++) s += i;
            V = s;
            for (int i = 0; i < 300_000; i++) W += 2;
        }
    }

    static final class InitBad {
        static long X;

        static {
            for (int i = 0; i < 300_000; i++) {
                if (i == 300_000 - 10) throw new IllegalStateException("boom " + i);
                X += i;
            }
        }
    }

    static String clinitThrow() {
        String first, second;
        try {
            sink += InitBad.X;
            first = "none";
        } catch (ExceptionInInitializerError e) {
            first = "EIIE " + e.getCause().getMessage();
        }
        try {
            sink += InitBad.X;
            second = "none";
        } catch (NoClassDefFoundError e) {
            second = "NCDFE " + e.getMessage();
        }
        return first + " | " + second;
    }

    // ── constructors before super(..) (JEP 513) ──────────────────────

    static class Base {
        final long v;

        Base(long v) {
            this.v = v;
        }
    }

    static final class Pre extends Base {
        long f;

        Pre(int n) {
            long s = 0;
            for (int i = 0; i < n; i++) s += i;
            super(s);
            for (int i = 0; i < n; i++) f += 1;
        }
    }

    static final class PreThrow extends Base {
        PreThrow(int n) {
            for (int i = 0; i < n; i++) {
                if (i == n - 1) throw new IllegalStateException("pre " + i);
            }
            super(0);
        }
    }

    public static void main(String[] args) {
        fact("osr-long-sum", longSum(N));
        fact("osr-long-counter", longCounter(N));
        fact("osr-double-sum", doubleSum(N));
        fact("osr-float", floatRow(N));
        fact("osr-two-slot-params", (long) (twoSlotParams(1L << 40, 0.25, N, 0.5f) * 4));
        fact("osr-neg-int-carried", negCarry(N, -7));
        fact("osr-slot-ref-then-long", slotRefThenLong(N));
        fact("osr-slot-highhalf-then-ints", slotHighHalfThenInts(N));
        fact("osr-top-local", topLocal(N, true) + " " + topLocal(N, false) + " "
                + topLocal(N, true) + " " + topLocal(N, false));
        fact("osr-top-local-int-gc", topLocalIntGc(N, true) + " " + topLocalIntGc(N, false)
                + " " + topLocalIntGc(N, true) + " " + topLocalIntGc(N, false));

        Holder h = new Holder();
        long hs = h.syncLoop(N);
        fact("osr-sync-method", hs + " in=" + tf(h.in) + " after=" + tf(h.after)
                + " caller=" + tf(Thread.holdsLock(h)) + " " + freeFor(h));
        Holder h2 = new Holder();
        String m2;
        hits = 0;
        try {
            h2.syncThrow(N);
            m2 = "none";
        } catch (IllegalStateException e) {
            m2 = e.getMessage();
        }
        fact("osr-sync-method-throw", m2 + " held=" + tf(Thread.holdsLock(h2)) + " hits=" + hits
                + " " + freeFor(h2));
        long ss = staticSyncLoop(N);
        fact("osr-static-sync", ss + " " + tf(staticIn) + tf(Thread.holdsLock(J27OsrStateRows.class))
                + " " + freeFor(J27OsrStateRows.class));
        Object a = new Object(), b = new Object();
        fact("osr-nested-sync", nested(a, b, N) + " " + freeFor(a, b));
        Object c = new Object(), d = new Object();
        fact("osr-nested-sync-throw", nestedThrow(c, d, N) + " " + freeFor(c, d));

        counter = 0;
        String m3;
        try {
            thrower(N);
            m3 = "none";
        } catch (IllegalStateException e) {
            m3 = e.getMessage();
        }
        fact("osr-exc-outer", m3 + " counter=" + counter);
        fact("osr-exc-in-loop", excInLoop(N));
        fact("osr-reentry-after-exit", reentry(100_000));
        fact("osr-cha-break-mid-loop", chaMid(N) + " " + chaMid(N));
        fact("osr-gc-only-locals", gcOnlyLocals(N));

        fact("osr-clinit", InitSum.V + " " + InitSum.W);
        fact("osr-clinit-throw", clinitThrow());

        Pre p = new Pre(N);
        fact("osr-ctor-pre-super", p.v + " " + p.f);
        String m4;
        try {
            new PreThrow(N);
            m4 = "none";
        } catch (IllegalStateException e) {
            m4 = e.getMessage();
        }
        fact("osr-ctor-pre-super-throw", m4);
    }
}
