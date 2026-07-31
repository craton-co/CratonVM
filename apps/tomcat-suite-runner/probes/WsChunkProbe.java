import java.nio.ByteBuffer;

/**
 * Isolates the per-completion work `WsFrameBase.processDataBinary` does for one
 * 8 KiB partial chunk on the CLIENT side of
 * `websocket.server.TestAsyncMessagesPerformance` (known-issue tomcat/32.3).
 *
 * Per chunk the client executes, with NO I/O in between:
 *   A  dest.put(inputBuffer)              -- NoopTransformation.getMoreData, bulk
 *   B  ByteBuffer.allocate(limit)         -- the defensive copy handed to onMessage
 *   C  copy.put(messageBufferBinary)      -- filling that copy
 *
 * 32.3's binding assertion (SEQ1) allows 0.5 ms between the two 8 KiB chunks of
 * one 16 KiB message, and measured ~0.7 ms. This probe says how much of that is
 * the three bulk buffer operations above rather than I/O.
 *
 * Args: [iters]
 */
public class WsChunkProbe {

    private static final int CHUNK = 8192;

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 20000;

        ByteBuffer input = ByteBuffer.allocate(CHUNK * 2 + 16);
        ByteBuffer dest = ByteBuffer.allocate(CHUNK);
        ByteBuffer directInput = ByteBuffer.allocateDirect(CHUNK * 2 + 16);
        byte[] scratch = new byte[CHUNK];

        long sink = 0;
        for (int round = 0; round < 3; round++) {
            System.out.println("--- round " + round + " (iters=" + iters + ") ---");

            // A: heap -> heap bulk put, exactly NoopTransformation.getMoreData
            long t0 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                input.clear();
                input.limit(CHUNK);
                dest.clear();
                dest.put(input);
                sink += dest.position();
            }
            long t1 = System.nanoTime();
            report("A heap->heap put(BB)   ", t1 - t0, iters);

            // A2: direct -> heap bulk put (what the socket read really produces)
            long t2 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                directInput.clear();
                directInput.limit(CHUNK);
                dest.clear();
                dest.put(directInput);
                sink += dest.position();
            }
            long t3 = System.nanoTime();
            report("A2 direct->heap put(BB)", t3 - t2, iters);

            // B: the per-chunk defensive allocation
            long t4 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                ByteBuffer copy = ByteBuffer.allocate(CHUNK);
                sink += copy.capacity();
            }
            long t5 = System.nanoTime();
            report("B allocate(8192)       ", t5 - t4, iters);

            // C: bulk array get/put, the shape arraycopy would take
            long t6 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                dest.clear();
                dest.limit(CHUNK);
                dest.get(scratch);
                sink += scratch[0];
            }
            long t7 = System.nanoTime();
            report("C get(byte[8192])      ", t7 - t6, iters);

            // D: raw System.arraycopy control
            byte[] src = new byte[CHUNK];
            byte[] dst = new byte[CHUNK];
            long t8 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                System.arraycopy(src, 0, dst, 0, CHUNK);
                sink += dst[0];
            }
            long t9 = System.nanoTime();
            report("D System.arraycopy     ", t9 - t8, iters);

            // E: the whole per-chunk sequence together
            long t10 = System.nanoTime();
            for (int i = 0; i < iters; i++) {
                directInput.clear();
                directInput.limit(CHUNK);
                dest.clear();
                dest.put(directInput);
                dest.flip();
                ByteBuffer copy = ByteBuffer.allocate(dest.limit());
                copy.put(dest);
                copy.flip();
                sink += copy.remaining();
            }
            long t11 = System.nanoTime();
            report("E full per-chunk       ", t11 - t10, iters);
        }
        System.out.println("sink=" + sink);
    }

    static void report(String label, long nanos, int iters) {
        System.out.println(label + " total=" + (nanos / 1000000L) + "ms  per-op="
                + (nanos / (double) iters / 1000.0) + "us");
    }
}
