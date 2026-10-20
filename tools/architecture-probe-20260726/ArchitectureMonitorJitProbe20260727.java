public final class ArchitectureMonitorJitProbe20260727 {
    private static final class Marker extends RuntimeException {
        private static final long serialVersionUID = 1L;
    }

    private static void fail() {
        throw new Marker();
    }

    private static int lockedMaybeFail(Object lock, int value) {
        synchronized (lock) {
            if (value < 0) {
                fail();
            }
            return value;
        }
    }

    private static long lockedAdd(Object lock, int value) {
        synchronized (lock) {
            return (value & 31) + 1L;
        }
    }

    private static long run(Object lock, int iterations) {
        long sum = 0;
        for (int i = 0; i < iterations; i++) {
            sum += lockedAdd(lock, i);
        }
        return sum;
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            throw new IllegalArgumentException("usage: ITERATIONS");
        }
        int iterations = Integer.parseInt(args[0]);
        Object lock = new Object();
        for (int i = 0; i < 100_000; i++) {
            lockedMaybeFail(lock, i);
        }
        boolean caught = false;
        try {
            lockedMaybeFail(lock, -1);
        } catch (Marker expected) {
            caught = true;
        }
        if (!caught || lockedMaybeFail(lock, 7) != 7) {
            throw new AssertionError("compiled synchronized cleanup did not rethrow/unlock");
        }
        run(lock, Math.min(iterations, 100_000));
        long started = System.nanoTime();
        long checksum = run(lock, iterations);
        long elapsed = System.nanoTime() - started;
        System.out.println("monitor-entry\t" + iterations + "\t" + elapsed + "\t" + checksum);
    }
}
