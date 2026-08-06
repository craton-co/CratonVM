import java.nio.ByteBuffer;

/**
 * Does a dropped `ByteBuffer.allocateDirect` ever give its off-heap bytes back?
 *
 * H2's `nioMemLZF:` filesystem allocates a fresh direct buffer per page
 * compress/expand and drops the old one, so `TestFileSystem.testConcurrent`
 * churns tens of thousands of them. If nothing reclaims them, the run dies with
 * `OutOfMemoryError: Direct buffer memory` long before the test finishes — which
 * is exactly what it does once the per-element throughput gap stops hiding it.
 *
 *   java DirectReclaimProbe [iterations] [blockBytes] [gcEvery]
 *
 * `gcEvery > 0` calls `System.gc()` on that stride, separating "reclamation
 * needs an explicit collection" from "reclamation never happens at all".
 * Passing 0 (the default) is the shape the JDK guarantees works unaided:
 * `Bits.reserveMemory` is contractually required to trigger reference
 * processing and retry before it throws.
 */
public class DirectReclaimProbe {

    public static void main(String[] args) {
        int iterations = args.length > 0 ? Integer.parseInt(args[0]) : 40000;
        int block = args.length > 1 ? Integer.parseInt(args[1]) : 65536;
        int gcEvery = args.length > 2 ? Integer.parseInt(args[2]) : 0;

        long t0 = System.nanoTime();
        int done = 0;
        try {
            for (int i = 0; i < iterations; i++) {
                ByteBuffer bb = ByteBuffer.allocateDirect(block);
                // Touch it so nothing can optimise the allocation away.
                bb.put(0, (byte) i);
                if (bb.get(0) != (byte) i) {
                    System.out.println("FAIL DirectReclaimProbe: readback mismatch at " + i);
                    System.exit(1);
                }
                done = i + 1;
                if (gcEvery > 0 && done % gcEvery == 0) {
                    System.gc();
                }
            }
        } catch (OutOfMemoryError e) {
            System.out.printf("FAIL DirectReclaimProbe: OOM after %d/%d buffers of %d bytes (%s)%n",
                    done, iterations, block, e.getMessage());
            System.out.printf("       reclaimed nothing after ~%d MiB of churn%n",
                    (long) done * block / (1024 * 1024));
            System.exit(1);
        }
        System.out.printf("OK DirectReclaimProbe: %d buffers of %d bytes (%d MiB churned) in %.1f ms%n",
                done, block, (long) done * block / (1024 * 1024),
                (System.nanoTime() - t0) / 1e6);
    }
}
