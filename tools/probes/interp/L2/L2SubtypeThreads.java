// Interpreter round i1 wave 5, lane L2 — lock-free subtype checks
// (published secondary-supers closures, `PublishedSupers`).
//
// Stdout is deterministic and must match HotSpot 25 exactly:
//   t1 poly=5333334 neg=0 catch=3000
//   t4 poly=21333336 neg=0 catch=12000
//
// What to measure (STDERR, "ns/iter" per phase): the same polymorphic
// interface `instanceof` + `checkcast` loop and a negative `instanceof` loop,
// first on 1 thread, then on 4 threads at once. Before wave 5 every question
// whose cast-site memo could not answer (a polymorphic site, a negative
// verdict) took the shared `class_manager` read lock — two atomic RMWs on one
// cache line every thread contends on — so the 4-thread ns/iter grew well past
// the 1-thread one. With the published closures the answer is one atomic load
// and a binary search, and the two should be close.
//
// Run with the JIT off (CRATONVM_DISABLE_JIT=1) — the interpreter's
// `op_instanceof` / `op_checkcast` are what changed — interleave binaries and
// take medians (this host's in-JVM timings swing ~3x between reps). A/B inside
// one binary: CRATONVM_LOADER_NO_SUBTYPE_DISPLAY=1 turns the display and the
// publication off (every question walks under the lock).
// `CRATONVM_DBG_FIELD_SITE=1` prints `cast: hit/neg_hit/miss/unusable` at exit.
public class L2SubtypeThreads {
    interface Shape { int size(); }
    interface Named { }
    static class Base implements Shape { public int size() { return 1; } }
    static final class Sq extends Base implements Named { }
    static final class Ci extends Base { }
    static final class Tri implements Shape { public int size() { return 1; } }
    static final class Other { }

    static final class Oops extends RuntimeException {
        Oops() { super(null, null, false, false); }
    }

    static final int N = 2_000_000;

    // Three receiver classes rotate through one site: the per-site receiver
    // memo keeps missing, so the hierarchy question is asked every time.
    static int poly(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            Object o = xs[i % 3];
            if (o instanceof Shape) s += ((Shape) o).size();
            if (o instanceof Named) s += 1;
        }
        return s;
    }

    // Negative against an interface nobody here implements, rotating receivers.
    static int neg(Object[] xs) {
        int s = 0;
        for (int i = 0; i < N; i++) {
            if (xs[i % 3] instanceof Runnable) s++;
        }
        return s;
    }

    // Exception matching: a catch row that refuses, then one that takes it.
    static int caught() {
        int c = 0;
        for (int i = 0; i < 1000; i++) {
            try {
                try {
                    throw new Oops();
                } catch (IllegalStateException e) {
                    c += 100;
                }
            } catch (RuntimeException e) {
                c++;
            }
        }
        return c;
    }

    static final Object[] XS = { new Sq(), new Ci(), new Tri() };
    static final Object[] YS = { new Sq(), new Other(), "s" };

    static long[] run() {
        long t0 = System.nanoTime();
        int p = poly(XS) + poly(XS);
        long t1 = System.nanoTime();
        int n = neg(YS);
        long t2 = System.nanoTime();
        int c = caught() + caught() + caught();
        return new long[] { p, n, c, t1 - t0, t2 - t1 };
    }

    static void phase(String label, int threads) throws Exception {
        long[][] out = new long[threads][];
        Thread[] ts = new Thread[threads];
        for (int k = 0; k < threads; k++) {
            final int kk = k;
            ts[k] = new Thread(() -> out[kk] = run());
        }
        for (Thread t : ts) t.start();
        for (Thread t : ts) t.join();
        long p = 0, n = 0, c = 0, tp = 0, tn = 0;
        for (long[] r : out) {
            p += r[0];
            n += r[1];
            c += r[2];
            tp = Math.max(tp, r[3]);
            tn = Math.max(tn, r[4]);
        }
        System.out.println(label + " poly=" + p + " neg=" + n + " catch=" + c);
        System.err.println(label + " poly ns/iter=" + (tp / (2.0 * N))
                + " neg ns/iter=" + (tn / (double) N));
    }

    public static void main(String[] a) throws Exception {
        run(); // warm the sites and publish the closures
        phase("t1", 1);
        phase("t4", 4);
    }
}
