// Lane L5 probe (round i1 wave 3): the two per-operation paths wave 3 made
// lock-free once warm.
//
// 1. `new` answered from the per-class allocation recipe
//    (`gc_and_alloc::gc_alloc_object` -> `jit::alloc_class_cache`): every
//    field of a freshly allocated object must read as its JVM default,
//    across a superclass chain with statics interleaved, on the first
//    allocation of the class (recipe built) and on every later one (recipe
//    read). A wrong slot index or a missing reference-slot write shows up as a
//    non-default value.
// 2. A `catch` that does NOT take the exception, crossed once per throw
//    (`exception_dispatch::catch_row_verdict`'s refusal memo,
//    `CastSite::negative_catch`): the outer handler must still catch every
//    throw, and the inner one none.
//
// HotSpot 25 prints (deterministic):
//   defaults ok 200000
//   caught outer 200000 inner 0
//   mixed outer 100000 inner 100000
// Timings go to stderr only; compare `new-ns` and `refuse-ns` against the
// previous binary (interleave runs, take medians).
public class WarmNewAndCatchRefusal {
    static class Base {
        int i;
        static long sBase = 7;
        Object o;
        boolean z;
    }

    static class Mid extends Base {
        long j;
        String s;
        static Object sMid = "x";
        float f;
    }

    static class Leaf extends Mid {
        double d;
        int[] a;
        char c;
        byte b;
        short h;
    }

    static boolean isDefault(Leaf x) {
        return x.i == 0 && x.o == null && !x.z
                && x.j == 0L && x.s == null && x.f == 0.0f
                && x.d == 0.0 && x.a == null && x.c == 0 && x.b == 0 && x.h == 0;
    }

    static int thrower(int k) {
        if ((k & 1) == 0) {
            throw new IllegalStateException("even");
        }
        throw new UnsupportedOperationException("odd");
    }

    // The inner `catch (IllegalArgumentException)` refuses every exception
    // thrown here; the outer one takes them.
    static int refuseThenCatch(int k) {
        try {
            try {
                return thrower(k * 2);
            } catch (IllegalArgumentException e) {
                return -1;
            }
        } catch (IllegalStateException e) {
            return 1;
        }
    }

    // Same row refuses one class and takes the other, alternately: a memo that
    // answered for the wrong class would miscount.
    static int mixed(int k) {
        try {
            try {
                return thrower(k);
            } catch (UnsupportedOperationException e) {
                return 2;
            }
        } catch (IllegalStateException e) {
            return 1;
        }
    }

    public static void main(String[] args) {
        final int n = 200_000;
        int ok = 0;
        long t0 = System.nanoTime();
        for (int k = 0; k < n; k++) {
            Leaf x = new Leaf();
            if (isDefault(x)) {
                ok++;
            }
            x.i = k;
            x.s = "dirty";
            x.a = new int[1];
        }
        long t1 = System.nanoTime();
        System.out.println("defaults ok " + ok);

        int outer = 0;
        int inner = 0;
        for (int k = 0; k < n; k++) {
            int r = refuseThenCatch(k);
            if (r == 1) {
                outer++;
            } else if (r == -1) {
                inner++;
            }
        }
        long t2 = System.nanoTime();
        System.out.println("caught outer " + outer + " inner " + inner);

        outer = 0;
        inner = 0;
        for (int k = 0; k < n; k++) {
            int r = mixed(k);
            if (r == 1) {
                outer++;
            } else if (r == 2) {
                inner++;
            }
        }
        System.out.println("mixed outer " + outer + " inner " + inner);
        System.err.println("new-ns " + (t1 - t0) / n + " refuse-ns " + (t2 - t1) / n);
    }
}
