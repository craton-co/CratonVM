import java.nio.ByteBuffer;
import java.nio.CharBuffer;

/**
 * Decomposes the cost of a forced-native NIO buffer accessor: a user virtual
 * call is the call-overhead floor, and the buffer accessors differ only in how
 * many by-name field resolutions their native body performs.
 */
public class BufCostProbe {

    static final int N = 2_000_000;

    static class Counter {
        int v;

        int bump() {
            return ++v;
        }
    }

    static void bench(String label, Runnable r) {
        r.run();
        long best = Long.MAX_VALUE;
        for (int i = 0; i < 3; i++) {
            long t = System.nanoTime();
            r.run();
            long d = System.nanoTime() - t;
            if (d < best) {
                best = d;
            }
        }
        System.out.printf("%-30s %8.1f ms  %7.1f ns/op%n", label, Double.valueOf(best / 1e6),
                Double.valueOf((double) best / N));
    }

    public static void main(String[] args) {
        final Counter c = new Counter();
        final ByteBuffer bb = ByteBuffer.allocate(4096);
        final CharBuffer cb = CharBuffer.allocate(4096);
        final int[] sink = new int[1];

        bench("user virtual call", () -> {
            for (int i = 0; i < N; i++) {
                sink[0] += c.bump();
            }
        });
        bench("bb.capacity()", () -> {
            for (int i = 0; i < N; i++) {
                sink[0] += bb.capacity();
            }
        });
        bench("bb.position()", () -> {
            for (int i = 0; i < N; i++) {
                sink[0] += bb.position();
            }
        });
        bench("bb.remaining()", () -> {
            for (int i = 0; i < N; i++) {
                sink[0] += bb.remaining();
            }
        });
        bench("bb.hasRemaining()", () -> {
            for (int i = 0; i < N; i++) {
                if (bb.hasRemaining()) {
                    sink[0]++;
                }
            }
        });
        bench("bb.get(int) absolute", () -> {
            for (int i = 0; i < N; i++) {
                sink[0] += bb.get(i & 4095);
            }
        });
        bench("bb.put(int,byte) absolute", () -> {
            for (int i = 0; i < N; i++) {
                bb.put(i & 4095, (byte) i);
            }
        });
        bench("bb.put(byte) relative", () -> {
            for (int i = 0; i < N; i++) {
                if (!bb.hasRemaining()) {
                    bb.position(0);
                }
                bb.put((byte) i);
            }
        });
        bench("cb.get(int) absolute", () -> {
            for (int i = 0; i < N; i++) {
                sink[0] += cb.get(i & 4095);
            }
        });
        System.out.println("sink=" + sink[0]);
    }
}
