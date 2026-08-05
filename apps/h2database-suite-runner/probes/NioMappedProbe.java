import org.h2.store.fs.FileUtils;

import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;

/**
 * Isolates `org.h2.store.fs.niomapped.FileNioMapped.unMap()` — the 10-second
 * "GC the MappedByteBuffer or fail" loop that `TestFileSystem` reaches once the
 * `nioMemLZF:` throughput gap stops stopping it earlier:
 *
 *   IOException: Timeout (10000 ms) reached while trying to GC mapped buffer
 *
 * H2 nulls its own `mapped` field, weakly references the buffer and spins on
 * `System.gc()` until the reference clears (the JDK-4724038 workaround). Each
 * `reMap()` after a size change does the same, so a handful of writes exercises
 * the loop several times.
 *
 *   java NioMappedProbe <dir> [rounds]
 */
public class NioMappedProbe {

    public static void main(String[] args) throws Exception {
        String dir = args.length > 0 ? args[0] : "/probe";
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 4;

        for (int round = 0; round < rounds; round++) {
            long t0 = System.nanoTime();
            String s = FileUtils.createTempFile("nioMapped:" + dir + "/tmp", ".tmp", false);
            FileChannel f = FileUtils.open(s, "rw");
            // Each growth triggers reMap(), i.e. unMap() of the previous mapping.
            for (int i = 0; i < 8; i++) {
                ByteBuffer bb = ByteBuffer.allocate(4096);
                bb.putInt(0, i);
                bb.position(0);
                f.write(bb, (long) i * 4096);
            }
            // Reads through the mapping, so the buffer is genuinely in use and
            // its reference has been through operand stacks and local slots.
            ByteBuffer rb = ByteBuffer.allocate(4);
            for (int i = 0; i < 8; i++) {
                rb.clear();
                f.read(rb, (long) i * 4096);
                rb.position(0);
                if (rb.getInt() != i) {
                    System.out.println("FAIL NioMappedProbe: readback mismatch at block " + i);
                    System.exit(1);
                }
            }
            f.close();
            FileUtils.delete(s);
            System.out.printf("round %d ok  %.1f ms%n", round, (System.nanoTime() - t0) / 1e6);
        }
        System.out.println("OK NioMappedProbe");
    }
}
