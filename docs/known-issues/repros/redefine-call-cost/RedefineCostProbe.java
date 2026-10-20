import java.io.InputStream;

/**
 * Measures what a JVMTI class redefinition costs *per subsequent call* into the
 * redefined class.
 *
 * This is the Mockito `SbCostProbe` reproducer with Mockito removed. Mockito's
 * inline mock maker is only a way to reach `Instrumentation.redefineClasses`;
 * the cost under investigation is the VM's, not the mocking library's. Dropping
 * it makes the probe runnable anywhere, in about a second, with no jars.
 *
 * The redefinition deliberately installs the **same bytes** the class already
 * has. Nothing about the class changes — same methods, same bodies, same
 * behaviour — so any slowdown afterwards is purely the cost of the VM having
 * marked the class redefined, with no behavioural difference to account for it.
 *
 * Run on both VMs and compare the within-run multiplier, not the absolute ns:
 *
 *   java  RedefineCostProbe        # HotSpot: redefine is a no-op here, ~1x
 *   cratonvm RedefineCostProbe
 */
public final class RedefineCostProbe {

    /** The class we redefine. Trivial on purpose: a hot, tiny, final method. */
    public static final class Victim {
        private int n;
        public int bump() {
            n = n + 1;
            return n;
        }
    }

    private static final int WARMUP = 200_000;
    private static final int CALLS  = 2_000_000;

    private static long timeCalls(Victim v, int iterations) {
        long t0 = System.nanoTime();
        int sink = 0;
        for (int i = 0; i < iterations; i++) {
            sink += v.bump();
        }
        long elapsed = System.nanoTime() - t0;
        if (sink == Integer.MIN_VALUE) {
            System.out.println("unreachable " + sink);
        }
        return elapsed;
    }

    /** Read `Victim`'s own class file back out of the classpath. */
    private static byte[] ownBytes() throws Exception {
        String resource = Victim.class.getName().replace('.', '/') + ".class";
        try (InputStream in = Victim.class.getClassLoader().getResourceAsStream(resource)) {
            if (in == null) {
                throw new IllegalStateException("cannot find " + resource + " on the classpath");
            }
            return in.readAllBytes();
        }
    }

    /**
     * Redefine through whichever door this VM offers.
     *
     * CratonVM exposes `cratonvm.Instrument.redefineClass(Class, byte[])`.
     * HotSpot needs a real java agent, which this probe does not set up — there
     * the redefine is skipped and the AFTER number is a control showing the
     * measurement loop itself is stable.
     */
    private static boolean redefine(byte[] bytes) {
        try {
            Class<?> bridge = Class.forName("cratonvm.Instrument");
            Object ok = bridge
                .getMethod("redefineClass", Class.class, byte[].class)
                .invoke(null, Victim.class, bytes);
            return Boolean.TRUE.equals(ok) || Integer.valueOf(1).equals(ok);
        } catch (ClassNotFoundException e) {
            System.out.println("NOTE  cratonvm.Instrument absent - redefine skipped (control run)");
            return false;
        } catch (Exception e) {
            System.out.println("NOTE  redefine failed: " + e);
            return false;
        }
    }

    public static void main(String[] args) throws Exception {
        Victim v = new Victim();
        timeCalls(v, WARMUP);

        long before = timeCalls(v, CALLS);
        long beforePer = before / CALLS;
        System.out.printf("BEFORE  %d bump() calls: %d ms  (%d ns/call)%n",
                CALLS, before / 1_000_000, beforePer);

        byte[] bytes = ownBytes();
        boolean redefined = redefine(bytes);
        System.out.println("REDEFINE applied=" + redefined + " (" + bytes.length + " identical bytes)");

        // Correctness check: the class must still work after being redefined
        // with its own bytes. A fast-but-wrong result is not a result.
        int probe = v.bump();
        if (probe <= 0) {
            throw new AssertionError("bump() returned " + probe + " after redefine");
        }

        long after = timeCalls(v, CALLS);
        long afterPer = after / CALLS;
        System.out.printf("AFTER   %d bump() calls: %d ms  (%d ns/call)%n",
                CALLS, after / 1_000_000, afterPer);

        if (beforePer > 0) {
            System.out.printf("MULTIPLIER %dx%n", afterPer / beforePer);
        } else {
            System.out.printf("MULTIPLIER n/a (before was %d ns/call)%n", beforePer);
        }
    }
}
