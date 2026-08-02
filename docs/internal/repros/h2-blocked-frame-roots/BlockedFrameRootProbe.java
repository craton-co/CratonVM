import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.Future;
import java.util.concurrent.TimeUnit;

/**
 * Does an object held ONLY by a Java frame local of a thread parked inside a
 * blocking call survive a young GC?
 *
 * Shape copied from org.h2.test.db.TestMultiThread.testConcurrentUpdate's
 * result loop, which produced
 *   NoSuchMethodError method="java/lang/Object.hasNext()Z"
 *      caller="...testConcurrentUpdate()V @pc=252"
 * on cratonvm: the enhanced-for's synthetic Iterator local read back as
 * ClassId(0) / num_fields=0 — the all-zero header the young sweep writes over
 * a span it reclaims.
 *
 * main() parks in Future.get() while N worker threads allocate hard, so every
 * young collection happens with main's frame blocked. Three things are held
 * only by main's frame across that window:
 *   - the enhanced-for Iterator (implicit, the H2 shape),
 *   - `canary`, a StringBuilder whose contents are verified afterwards,
 *   - `jobs` itself.
 *
 * Usage: BlockedFrameRootProbe [rounds] [threads] [allocMB]
 */
public class BlockedFrameRootProbe {

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 200;
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 8;
        final int allocMB = args.length > 2 ? Integer.parseInt(args[2]) : 8;

        int bad = 0;
        for (int r = 0; r < rounds; r++) {
            ExecutorService ex = Executors.newFixedThreadPool(threads);
            List<Future<Void>> jobs = new ArrayList<>();
            List<Callable<Void>> callables = new ArrayList<>();
            for (int i = 0; i < threads; i++) {
                callables.add(() -> {
                    // Allocate hard so young collections fire while main is parked.
                    for (int k = 0; k < allocMB * 64; k++) {
                        byte[] b = new byte[16 * 1024];
                        b[0] = (byte) k;
                        sink = b;
                        StringBuilder sb = new StringBuilder();
                        for (int q = 0; q < 40; q++) {
                            sb.append(q).append(',');
                        }
                        sink = sb.toString();
                    }
                    return null;
                });
            }
            for (int i = 0; i < threads; i++) {
                jobs.add(ex.submit(callables.get(i)));
            }

            StringBuilder canary = new StringBuilder("CANARY-" + r);
            String expect = canary.toString();

            // The frame local under test: the enhanced-for iterator, live across
            // every job.get() below.
            for (Future<Void> job : jobs) {
                job.get(5, TimeUnit.MINUTES);
            }

            String got = canary.toString();
            if (!expect.equals(got)) {
                bad++;
                System.out.println("round " + r + " CANARY CORRUPT expect=" + expect + " got=" + got);
            }
            if (jobs.size() != threads) {
                bad++;
                System.out.println("round " + r + " JOBS LIST CORRUPT size=" + jobs.size());
            }
            ex.shutdown();
            ex.awaitTermination(60, TimeUnit.SECONDS);
            if (r % 20 == 0) {
                System.out.println("round " + r + " ok");
            }
        }
        System.out.println("bad=" + bad);
        System.out.println("DONE");
    }
}
