import java.nio.ByteBuffer;

/**
 * Are unreachable direct {@link ByteBuffer}s reclaimed?
 *
 * H2's MVStore allocates one direct buffer per chunk write and drops it; if the
 * VM never releases them, `MaxDirectMemorySize` (which defaults to `-Xmx`) is
 * exhausted and the store panics with
 * `OutOfMemoryError: Direct buffer memory`.
 */
public final class DirectBufProbe {
    public static void main(String[] args) throws Exception {
        int mb = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 400;
        long sink = 0;
        for (int i = 0; i < rounds; i++) {
            ByteBuffer b = ByteBuffer.allocateDirect(mb * 1024 * 1024);
            b.putInt(0, i);
            sink += b.getInt(0);
            // dropped here; nothing else references it
            if (i % 50 == 0) {
                System.out.println("round " + i + " allocated " + ((long) (i + 1) * mb) + " MiB total");
            }
        }
        System.out.println("OK rounds=" + rounds + " perBufMiB=" + mb
                + " totalAllocatedMiB=" + ((long) rounds * mb) + " sink=" + sink);
    }
}
