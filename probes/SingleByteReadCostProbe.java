import java.io.BufferedInputStream;
import java.io.ByteArrayInputStream;
import java.io.DataInputStream;
import java.io.IOException;
import java.util.concurrent.locks.ReentrantLock;

/**
 * Decomposes the cost of one `InputStream.read()` call, which is what Tomcat's
 * BCEL class parser -- and therefore a webapp deploy -- spends its time in.
 *
 * `AnnotationScanCostProbe` prices the deploy's real work at 3705 us/class on
 * CratonVM vs 16.4 us/class on HotSpot (226x) while JAR *reading* is only 3.6x
 * slower, so the cost is the per-byte parse, not the I/O. JDK 25's
 * `BufferedInputStream.read()` is
 *
 *     if (lock != null) { lock.lock(); try { return implRead(); } finally { lock.unlock(); } }
 *     else              { synchronized (this) { return implRead(); } }
 *
 * so each byte pays a lock round trip plus a call chain. This probe times each
 * layer against a floor so the cost can be attributed.
 *
 * HARNESS NOTE (this bit is load-bearing): every measured loop lives in its own
 * named static method that `main` calls directly. An earlier version put the
 * loops in lambda bodies behind a functional interface; those bodies run only
 * a handful of times, so they never reach the invocation threshold and the
 * whole measurement read the interpreter -- a trivial `return field;` call
 * "cost" 510 ns/op, which is a fact about the harness, not about the VM. Do not
 * reintroduce a lambda/interface indirection here.
 */
public class SingleByteReadCostProbe {

    private static final int N = 200_000;
    private static final int ROUNDS = 8;
    private static final ReentrantLock LOCK = new ReentrantLock();
    private static final Object MON = new Object();
    private static int field = 1;
    private static byte[] payload;

    public static void main(String[] args) throws Exception {
        payload = new byte[N];
        for (int i = 0; i < N; i++) {
            payload[i] = (byte) i;
        }

        long[] r = new long[6];
        long sink = 0;
        // Interleave the rounds so every shape sees the same warm-up history
        // and the same background-compiler state.
        for (int round = 0; round < ROUNDS; round++) {
            sink += bench(0, r, round);
            sink += bench(1, r, round);
            sink += bench(2, r, round);
            sink += bench(3, r, round);
            sink += bench(4, r, round);
            sink += bench(5, r, round);
        }

        report("floor: static call returning a field", r[0]);
        report("ReentrantLock lock()/unlock() round trip", r[1]);
        report("synchronized(obj) enter/exit round trip", r[2]);
        report("ByteArrayInputStream.read()", r[3]);
        report("BufferedInputStream.read() over BAIS", r[4]);
        report("DataInputStream.readUnsignedByte over BIS/BAIS", r[5]);
        System.out.println("(sink=" + sink + ")");
    }

    private static long bench(int which, long[] best, int round) throws IOException {
        long t0 = System.nanoTime();
        long s;
        switch (which) {
            case 0 -> s = floorLoop();
            case 1 -> s = lockLoop();
            case 2 -> s = syncLoop();
            case 3 -> s = baisLoop();
            case 4 -> s = bisLoop();
            default -> s = disLoop();
        }
        long dt = System.nanoTime() - t0;
        // Discard the first two rounds entirely: they are warm-up.
        if (round >= 2 && (best[which] == 0 || dt < best[which])) {
            best[which] = dt;
        }
        return s;
    }

    private static void report(String label, long ns) {
        System.out.printf("%-48s %10.1f ns/op   (%8.1f ms / %d ops)%n", label, (double) ns / N, ns / 1e6, N);
    }

    private static int floor() {
        return field;
    }

    private static long floorLoop() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += floor();
        }
        return s;
    }

    private static long lockLoop() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            LOCK.lock();
            try {
                s += field;
            } finally {
                LOCK.unlock();
            }
        }
        return s;
    }

    private static long syncLoop() {
        long s = 0;
        for (int i = 0; i < N; i++) {
            synchronized (MON) {
                s += field;
            }
        }
        return s;
    }

    private static long baisLoop() throws IOException {
        ByteArrayInputStream in = new ByteArrayInputStream(payload);
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += in.read();
        }
        return s;
    }

    private static long bisLoop() throws IOException {
        BufferedInputStream in = new BufferedInputStream(new ByteArrayInputStream(payload));
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += in.read();
        }
        return s;
    }

    private static long disLoop() throws IOException {
        DataInputStream in = new DataInputStream(new BufferedInputStream(new ByteArrayInputStream(payload)));
        long s = 0;
        for (int i = 0; i < N; i++) {
            s += in.readUnsignedByte();
        }
        return s;
    }
}
