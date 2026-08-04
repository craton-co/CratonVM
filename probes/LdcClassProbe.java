import java.util.ArrayList;
import java.util.List;

/**
 * Correctness probe for compiled `ldc <Class>`.
 *
 * Each stage is a small static method whose whole body is one `ldc <Class>`,
 * called often enough to be JIT-compiled, and checked on every iteration:
 *
 *  * {@code hotLoaded}   — a class that is certainly loaded before the compile.
 *  * {@code hotCold}     — a class first referenced HERE, so the site is
 *    compiled while its target is still unloaded and must resolve at run time.
 *  * {@code hotArray}    — an array class literal, whose mirror is synthesised
 *    rather than read off a loaded class file.
 *  * {@code hotPrimitive}— {@code int.class}, which javac emits as a
 *    {@code getstatic Integer.TYPE}, not an `ldc`; it is here as the control
 *    that says the checks themselves are not vacuous.
 *
 * The loop allocates so a moving young collection can happen between two
 * executions of the same compiled body: a mirror baked as an immediate would
 * survive the identity check for a while and then start returning a relocated
 * (or reused) address, so identity is asserted against a reference taken
 * BEFORE the loop, not against the previous iteration.
 */
public final class LdcClassProbe {

    /** Referenced only by {@link #hotCold()}, so it is unloaded at compile time. */
    static final class ColdOnly {
        int x;
    }

    private static Class<?> hotLoaded() {
        return String.class;
    }

    private static Class<?> hotCold() {
        return ColdOnly.class;
    }

    private static Class<?> hotArray() {
        return byte[].class;
    }

    private static Class<?> hotPrimitive() {
        return int.class;
    }

    public static void main(String[] args) {
        int iters = Integer.getInteger("probe.iters", 200000);

        Class<?> expectedLoaded = String.class;
        Class<?> expectedArray = byte[].class;
        Class<?> expectedPrimitive = int.class;
        // Deliberately NOT `ColdOnly.class` here — taking it would load the
        // class before the compile and defeat the point of the stage.

        List<byte[]> churn = new ArrayList<>();
        long sink = 0;
        for (int i = 0; i < iters; i++) {
            Class<?> a = hotLoaded();
            if (a != expectedLoaded) {
                throw new AssertionError("hotLoaded returned " + a + " at i=" + i);
            }
            Class<?> b = hotArray();
            if (b != expectedArray || !b.getName().equals("[B")) {
                throw new AssertionError("hotArray returned " + b + " at i=" + i);
            }
            Class<?> c = hotPrimitive();
            if (c != expectedPrimitive) {
                throw new AssertionError("hotPrimitive returned " + c + " at i=" + i);
            }
            Class<?> d = hotCold();
            if (!"LdcClassProbe$ColdOnly".equals(d.getName())) {
                throw new AssertionError("hotCold returned " + d + " at i=" + i);
            }
            sink += a.getName().length() + d.getName().length();

            // Allocation pressure, retained in a bounded ring so young
            // collections actually run and actually relocate.
            churn.add(new byte[64]);
            if (churn.size() > 256) {
                churn.clear();
            }
        }
        // Identity must also hold across the whole run, not just per iteration.
        if (hotCold() != hotCold()) {
            throw new AssertionError("hotCold is not identity-stable");
        }
        System.out.println("LdcClassProbe OK iters=" + iters + " sink=" + sink);
    }
}
