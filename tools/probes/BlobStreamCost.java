import java.io.IOException;
import java.io.InputStream;
import java.util.Random;

/** Second decomposition: separate the Random-constructor cost from the read() body's. */
public class BlobStreamCost {

    static final int ITERS = Integer.getInteger("iters", 300_000);
    static long sink;

    interface Arm { long run(int n) throws IOException; }

    static void time(String name, Arm arm) throws IOException {
        arm.run(Math.min(ITERS / 10, 100_000));
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 3; r++) {
            long t0 = System.nanoTime();
            sink += arm.run(ITERS);
            long dt = System.nanoTime() - t0;
            if (dt < best) best = dt;
        }
        System.out.printf("CK %-30s %8.1f ns/op%n", name, (double) best / ITERS);
    }

    // --- the constructor split ---------------------------------------
    static long armNewRandomSeeded(int n) {          // no entropy draw
        long a = 0;
        for (int i = 0; i < n; i++) a += new Random(i).nextInt();
        return a;
    }
    static long armNewRandomNoSeed(int n) {          // entropy draw per ctor
        long a = 0;
        for (int i = 0; i < n; i++) a += new Random().nextInt();
        return a;
    }

    // --- the counter split -------------------------------------------
    static long armBoxedCounter(int n) {
        Long count = (long) n;
        long a = 0;
        for (int i = 0; i < n; i++) { if (count > 0) { count--; a++; } }
        return a;
    }
    static long armPrimCounter(int n) {
        long count = n;
        long a = 0;
        for (int i = 0; i < n; i++) { if (count > 0) { count--; a++; } }
        return a;
    }

    // --- the two streams, identical but for the counter's type -------
    static final class BoxedStream extends InputStream {
        private boolean read = false;
        private Long count;
        BoxedStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return new Random().nextInt(); }
            return -1;
        }
        boolean wasRead() { return read; }
    }
    static final class PrimStream extends InputStream {
        private boolean read = false;
        private long count;
        PrimStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return new Random().nextInt(); }
            return -1;
        }
        boolean wasRead() { return read; }
    }
    /** Same shape again, with the Random hoisted out — isolates dispatch+counter. */
    static final class PrimSharedRandomStream extends InputStream {
        private boolean read = false;
        private long count;
        private final Random rnd = new Random(7);
        PrimSharedRandomStream(long n) { this.count = n; }
        @Override public int read() {
            read = true;
            if (count > 0) { count--; return rnd.nextInt(); }
            return -1;
        }
        boolean wasRead() { return read; }
    }

    static long armBoxedStream(int n) throws IOException {
        BoxedStream in = new BoxedStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }
    static long armPrimStream(int n) throws IOException {
        PrimStream in = new PrimStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }
    static long armPrimSharedStream(int n) throws IOException {
        PrimSharedRandomStream in = new PrimSharedRandomStream(n);
        long a = 0;
        for (int i = 0; i < n; i++) a += in.read();
        return a + (in.wasRead() ? 1 : 0);
    }

    public static void main(String[] args) throws Exception {
        System.out.println("CK BlobStreamCost iters=" + ITERS);
        time("new Random(seed).nextInt", BlobStreamCost::armNewRandomSeeded);
        time("new Random().nextInt", BlobStreamCost::armNewRandomNoSeed);
        time("boxed Long counter", BlobStreamCost::armBoxedCounter);
        time("primitive long counter", BlobStreamCost::armPrimCounter);
        time("stream boxed+new Random", BlobStreamCost::armBoxedStream);
        time("stream prim +new Random", BlobStreamCost::armPrimStream);
        time("stream prim +shared Random", BlobStreamCost::armPrimSharedStream);
        System.out.println("CK BlobStreamCost sink=" + (sink == 0 ? 0 : 1));
    }
}
