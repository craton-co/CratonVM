// Declares the package its directory implies — see the note in
// ManifestNullValue.java for why a `cratonvm/`-resident source that declares
// no package produces a `.class` whose `this_class` contradicts its path.
package cratonvm;

public final class JitSafepointStress {
    private static volatile boolean stop;
    private static volatile long sink;

    private static long pureLoop(long seed, int iterations) {
        long value = seed;
        for (int i = 0; i < iterations; i++) {
            value = value * 6364136223846793005L + 1442695040888963407L;
            if ((value & 1L) == 0L) {
                value ^= 0x5deece66dL;
            }
        }
        return value;
    }

    private static final class Worker extends Thread {
        @Override
        public void run() {
            long value = 1L;
            while (!stop) {
                // Once warmed, this remains in a pure compiled loop long enough
                // for the peer to request several stop-the-world collections.
                value = pureLoop(value, 2_000_000);
            }
            sink = value;
        }
    }

    public static void main(String[] args) throws Exception {
        long warm = 1L;
        for (int i = 0; i < 300; i++) {
            warm = pureLoop(warm, 10_000);
        }

        Worker worker = new Worker();
        worker.start();
        long allocationChecksum = 0L;
        for (int round = 0; round < 20; round++) {
            for (int i = 0; i < 20_000; i++) {
                byte[] bytes = new byte[256];
                bytes[0] = (byte) (round + i);
                allocationChecksum += bytes[0];
            }
            System.gc();
        }
        stop = true;
        worker.join(15_000L);
        if (worker.isAlive()) {
            throw new AssertionError("compiled worker did not reach a safepoint");
        }
        System.out.println(
                "JIT_SAFEPOINT_STRESS_OK " + (warm ^ sink ^ allocationChecksum));
    }
}
