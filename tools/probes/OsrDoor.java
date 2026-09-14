/**
 * H12-1's independent-reproduction probe and H20-1 section 4's acceptance
 * measurement, transcribed from the records: a single `static long hot()`
 * called EXACTLY ONCE containing a 300,000-iteration loop over
 * Thread.currentThread().hashCode().
 *
 * Called once, so the MethodEntry door cannot be what tiers it up.
 * 300,000 iterations, so OSR must.
 *
 * Read with CRATONVM_INTRINSIC_STATS=1; the number that matters is the
 * Thread.currentThread direct-call count.
 */
public class OsrDoor {
    static long hot() {
        long acc = 0;
        for (int i = 0; i < 300_000; i++) {
            acc += Thread.currentThread().hashCode();
        }
        return acc;
    }

    public static void main(String[] a) {
        long v = hot();
        System.out.println("OsrDoor done nonzero=" + (v != 0));
    }
}
