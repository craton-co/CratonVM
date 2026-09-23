import java.security.MessageDigest;

/**
 * The `digest` phase of HugeByteLoopProbe with netty removed: one
 * MessageDigest.update(byte) per byte, in an OSR-compiled inner loop, and
 * nothing else in the window.
 *
 * <p>HugeByteLoopProbe needs the netty suite's classpath (PooledByteBufAllocator,
 * ByteProcessor) even in its `digest` phase, where the ByteBuf is allocated and
 * never written. This probe drops that dependency so the
 * `jdkonly-thin-jit-direct-helpers-refuse-what-the-tail-then-runs-20260922`
 * A/B can be re-run on any host. The loop it measures is identical: the same
 * receiver for every byte, so `direct_receiver_facts`' per-epoch memo hits on
 * all but the first, and `jit_md_update_byte_direct` is the whole body.
 *
 * <p>Usage: {@code MdUpdateByteLoopProbe [mebibytes]} (default 8).
 */
public class MdUpdateByteLoopProbe {
    public static void main(String[] args) throws Exception {
        int chunkSize = 1024 * 1024;
        int chunks = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        MessageDigest digest = MessageDigest.getInstance("SHA-256");
        long t0 = System.nanoTime();
        for (int i = 0; i < chunks; i++) {
            for (int j = 0; j < chunkSize; j++) {
                digest.update((byte) (i + (j & 0xA0)));
            }
        }
        long ms = (System.nanoTime() - t0) / 1000000L;
        System.out.println("PROBE chunks=" + chunks + " bytes=" + ((long) chunks * chunkSize)
                + " ms=" + ms + " d0=" + digest.digest()[0]);
    }
}
