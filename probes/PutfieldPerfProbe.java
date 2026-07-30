/**
 * Sharpest possible measurement of a null check added to the putfield lowering:
 * a loop that does almost nothing BUT field stores, on a non-null receiver.
 *
 * Any per-store cost shows up here magnified far beyond what real code would
 * see, so this is a worst case, not a representative one.
 */
public final class PutfieldPerfProbe {

    static final class Holder {
        int a, b, c, d;
        long wide;
        Object ref;
    }

    static void storeInts(Holder h, int v) {
        h.a = v;
        h.b = v + 1;
        h.c = v + 2;
        h.d = v + 3;
    }

    static void storeWide(Holder h, long v) {
        h.wide = v;
    }

    static void storeRef(Holder h, Object v) {
        h.ref = v;
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 20_000_000 : Integer.parseInt(args[0]);
        Holder h = new Holder();
        Object payload = "p";

        // warm
        for (int i = 0; i < 200_000; i++) {
            storeInts(h, i);
            storeWide(h, i);
            storeRef(h, payload);
        }

        long t0 = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            storeInts(h, i);
        }
        long tInts = System.nanoTime() - t0;

        t0 = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            storeWide(h, i);
        }
        long tWide = System.nanoTime() - t0;

        t0 = System.nanoTime();
        for (int i = 0; i < iterations; i++) {
            storeRef(h, payload);
        }
        long tRef = System.nanoTime() - t0;

        System.out.println("ints=" + tInts + " wide=" + tWide + " ref=" + tRef
                + " checksum=" + (h.a + h.b + h.c + h.d + h.wide + h.ref.hashCode()));
    }
}
