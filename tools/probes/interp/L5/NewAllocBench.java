/*
 * Interpreter round i1, wave 16, lane L2: the cost of an interpreted `new`
 * after its site cache hit (`op_new` -> `gc_alloc_object`).
 *
 * What the stage should speed up: `gc_alloc_object` no longer writes the
 * per-class default recipe into a COMPACT body carved from a TLAB
 * (`compact_body_holds_defaults`): a compact slot is tagless and the carved
 * chunk is zeroed, so every `long`/`float`/`double`/reference default the
 * recipe stored was a zero written over a zero, one `VmHeap::set_field`
 * (header read, bounds and array checks, layout lookup, store) per field.
 * The classes below have 7 such fields per `Sub`, 2 per `Base` and 3 per
 * `Pair` (int-family fields were never stored, since wave 4), and a
 * finalizer-free hierarchy, so the warm path is: plan the shape, bump the
 * TLAB, stamp the header, read the lock-free recipe, done.
 *
 * Measure with `--nojit` (the interpreter's `new`), interleaved against the
 * previous build; timings (ns per allocation, per round) go to stderr. With
 * the JIT on the loops compile and the number measures the compiled `new`
 * instead, which this stage does not touch.
 *
 * stdout is deterministic and identical on HotSpot 25:
 *
 *   defaults nonZero=0 nonNullRefs=0
 *   checksum=254996230
 */
public class NewAllocBench {
    static class Base {
        int i;
        long l;
        Object ref;
    }

    static final class Sub extends Base {
        double d;
        float f;
        String s;
        int[] arr;
        boolean z;
        long l2;
    }

    static final class Pair {
        Object left;
        Object right;
        long tag;
    }

    static int nonZero;
    static int nonNullRefs;

    static long checkDefaults(Sub o) {
        if (o.i != 0) nonZero++;
        if (o.l != 0L) nonZero++;
        if (o.d != 0.0) nonZero++;
        if (o.f != 0.0f) nonZero++;
        if (o.z) nonZero++;
        if (o.l2 != 0L) nonZero++;
        if (o.ref != null) nonNullRefs++;
        if (o.s != null) nonNullRefs++;
        if (o.arr != null) nonNullRefs++;
        return 1;
    }

    static long checkDefaults(Pair p) {
        if (p.tag != 0L) nonZero++;
        if (p.left != null) nonNullRefs++;
        if (p.right != null) nonNullRefs++;
        return 1;
    }

    /** One round: `n` iterations, three allocations each. */
    static long round(int n) {
        long sum = 0;
        Pair keep = null;
        for (int k = 0; k < n; k++) {
            Sub s = new Sub();
            Base b = new Base();
            Pair p = new Pair();
            // Check the defaults on a sample only, so the loop is dominated by
            // the allocations; every read still sees a never-written object.
            if ((k & 1023) == 0) {
                sum += checkDefaults(s);
                sum += checkDefaults(p);
                if (b.i != 0 || b.l != 0L) nonZero++;
                if (b.ref != null) nonNullRefs++;
            }
            s.i = k;
            s.l2 = k * 3L;
            b.l = k;
            p.left = s;
            p.right = keep;
            p.tag = s.i + s.l2 + b.l;
            sum += p.tag & 0xff;
            // A short chain keeps a few objects alive across TLAB refills.
            keep = ((k & 15) == 0) ? null : p;
        }
        return sum;
    }

    public static void main(String[] args) {
        final int rounds = 5;
        final int n = 400_000;
        long checksum = 0;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            checksum += round(n);
            long t1 = System.nanoTime();
            System.err.println("round " + r + ": "
                + ((t1 - t0) / (3L * n)) + " ns/alloc");
        }
        System.out.println("defaults nonZero=" + nonZero + " nonNullRefs=" + nonNullRefs);
        System.out.println("checksum=" + checksum);
    }
}
