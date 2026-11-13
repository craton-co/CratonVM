/*
 * Interpreter round i1, wave 5, lane L5: a freshly allocated object's
 * int-family fields read as zero on every allocation path, now that neither
 * the interpreter's `new` (wave 4) nor the JIT's post-allocation helper
 * (wave 5, `jit_post_alloc_init`) STORES those defaults -- they rely on the
 * allocator's zeroed body -- and an int-only class skips the JIT helper call
 * altogether (`jit_post_alloc_init_is_noop`).
 *
 * The loop runs long enough to be compiled (and OSR'd), so the same `new`
 * sites execute interpreted, through the inline TLAB bump, and through the
 * slow-path helper when a TLAB fills. Mixed classes (int + long/double +
 * reference) check the tagged defaults are still written beside the skipped
 * int ones, and a finalizable int-only class checks the helper still runs for
 * the finalizer registration.
 *
 * HotSpot 25 prints exactly (and CratonVM must, with and without --nojit):
 *
 *   intOnly nonZero=0
 *   mixed nonZero=0 nonNullRefs=0
 *   subclass nonZero=0
 *   finalizable nonZero=0
 *   checksum=8000000
 *
 * Timing goes to stderr (allocations per ms of the int-only loop), for an
 * interleaved before/after comparison of the helper-call skip.
 */
public class L5IntOnlyAllocDefaults {
    static final class IntOnly {
        int a;
        boolean b;
        char c;
        byte d;
        short e;
    }

    static final class Mixed {
        int i;
        long l;
        double d;
        Object ref;
        boolean z;
    }

    static class Base {
        int x;
        short y;
    }

    static final class Sub extends Base {
        char z;
        boolean w;
    }

    static final class Fin {
        int q;
        byte r;

        @Override
        @SuppressWarnings("removal")
        protected void finalize() {}
    }

    static int intOnlyNonZero(int n) {
        int bad = 0;
        for (int k = 0; k < n; k++) {
            IntOnly o = new IntOnly();
            if (o.a != 0 || o.b || o.c != 0 || o.d != 0 || o.e != 0) bad++;
            o.a = k;
            o.b = true;
            o.c = 'x';
        }
        return bad;
    }

    static int mixedNonZero(int n, int[] nonNullRefs) {
        int bad = 0;
        for (int k = 0; k < n; k++) {
            Mixed m = new Mixed();
            if (m.i != 0 || m.l != 0L || m.d != 0.0 || m.z) bad++;
            if (m.ref != null) nonNullRefs[0]++;
            m.ref = m;
        }
        return bad;
    }

    static int subNonZero(int n) {
        int bad = 0;
        for (int k = 0; k < n; k++) {
            Sub s = new Sub();
            if (s.x != 0 || s.y != 0 || s.z != 0 || s.w) bad++;
        }
        return bad;
    }

    static int finNonZero(int n) {
        int bad = 0;
        for (int k = 0; k < n; k++) {
            Fin f = new Fin();
            if (f.q != 0 || f.r != 0) bad++;
        }
        return bad;
    }

    public static void main(String[] args) {
        final int n = 2_000_000;
        long t0 = System.nanoTime();
        int intOnly = intOnlyNonZero(n);
        long t1 = System.nanoTime();
        int[] nonNull = new int[1];
        int mixed = mixedNonZero(n, nonNull);
        int sub = subNonZero(n);
        int fin = finNonZero(20_000);
        System.out.println("intOnly nonZero=" + intOnly);
        System.out.println("mixed nonZero=" + mixed + " nonNullRefs=" + nonNull[0]);
        System.out.println("subclass nonZero=" + sub);
        System.out.println("finalizable nonZero=" + fin);
        System.out.println("checksum=" + (4L * n + intOnly + mixed + sub + fin + nonNull[0]));
        double ms = (t1 - t0) / 1e6;
        System.err.printf("intOnly: %d allocs in %.1f ms (%.0f allocs/ms)%n", n, ms, n / ms);
    }
}
