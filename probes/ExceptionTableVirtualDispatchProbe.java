/**
 * Regression for direct virtual dispatch into a compiled callee that owns a
 * catch block. Normal calls must use the cached compiled entry; a later bounds
 * failure must still be routed through that callee's catch rather than escape
 * to its caller.
 */
public final class ExceptionTableVirtualDispatchProbe {
    interface Lookup {
        int get(int index);
    }

    static final class CatchingLookup implements Lookup {
        private final int[] values = { 3, 5, 7, 11 };

        @Override
        public int get(int index) {
            try {
                return values[index];
            } catch (ArrayIndexOutOfBoundsException expected) {
                return -1;
            }
        }
    }

    private static final Lookup LOOKUP = new CatchingLookup();
    private static volatile long sink;

    private static long run(int iterations) {
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
            sum += LOOKUP.get(i & 3);
        }
        sink = sum;
        return sum;
    }

    public static void main(String[] args) {
        int iterations = args.length == 0 ? 500_000 : Integer.parseInt(args[0]);
        // Warm both the interface caller and the exception-table callee before
        // measuring the monomorphic cached-entry path.
        run(100_000);
        long start = System.nanoTime();
        long sum = run(iterations);
        long elapsed = System.nanoTime() - start;

        if (sum <= 0 || LOOKUP.get(-1) != -1 || LOOKUP.get(4) != -1) {
            throw new AssertionError("callee catch was not preserved");
        }
        System.out.println("EXCEPTION_TABLE_VIRTUAL_OK ns/op=" +
                (elapsed / (double) iterations) + " sink=" + sink);
    }
}
