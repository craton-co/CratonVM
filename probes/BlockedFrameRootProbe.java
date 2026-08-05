import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.Callable;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.TimeUnit;

/**
 * Blocked-thread frame-root probe — the invariant behind
 * docs/known-issues/hibernate/smoketests-stale-pointer-nosuchmethod-crash-20260804.md,
 * reached at its own rate instead of through a three-minute Hibernate class.
 *
 * <p>{@code SmokeTests#testQueryConcurrency} parks the JUnit main thread in
 * {@code ExecutorService.invokeAll} under a ~45-frame JUnit stack while five
 * workers churn the young generation. A thread inside a blocked region is
 * EXCLUDED from every stop-the-world census, so a moving young collection
 * completes without it; the only thing that keeps its frames usable is the
 * initiator-side fold ({@code ThreadRegistry::fold_pointer_map_into_blocked})
 * plus the wake-side apply ({@code check_post_block_gc}). If either misses a
 * slot, the frame resumes holding a vacated from-space address, which reads
 * back as an all-zero {@code java.lang.Object} header — and the failure only
 * surfaces much later, as a {@code NoSuchMethodError} on
 * {@code java.lang.Object} or an out-of-bounds field read.
 *
 * <p>This probe reproduces exactly that shape and CHECKS it, rather than
 * waiting for a downstream crash:
 *
 * <ul>
 *   <li>a deep recursive stack, each level holding four distinct object
 *       locals that are read back AFTER the blocking call (so per-bci local
 *       liveness must keep them rooted across the whole blocked window);</li>
 *   <li>the innermost level blocks in {@code invokeAll} many times while the
 *       pool threads allocate hard enough to force young collections;</li>
 *   <li>on the way back out every local is verified by identity, by field
 *       value, and by a virtual call — the three faces a reclaimed or
 *       un-remapped receiver fails.</li>
 * </ul>
 *
 * <p>Exit status is the number of corrupted locals (0 = clean), and every
 * corruption is printed with the frame depth and slot so a hit names the
 * missing root.
 *
 * <pre>
 *   javac -d probes/out probes/BlockedFrameRootProbe.java
 *   cratonvm --java-home &lt;jdk&gt; --Xmx 256m -cp probes/out BlockedFrameRootProbe
 *   # knobs: -Ddepth=40 -Drounds=200 -Dtasks=64 -Dpool=5 -Dchurn=2000
 * </pre>
 */
public final class BlockedFrameRootProbe {

    static final int DEPTH = Integer.getInteger("depth", 40);
    /** Whole deep-stack + blocked-window + verify cycles. */
    static final int PASSES = Integer.getInteger("passes", 200);
    static final int ROUNDS = Integer.getInteger("rounds", 4);
    static final int TASKS = Integer.getInteger("tasks", 64);
    static final int POOL = Integer.getInteger("pool", 5);
    static final int CHURN = Integer.getInteger("churn", 2000);

    static int corrupted = 0;
    static int checked = 0;

    /** A payload with three independently-checkable faces. */
    static final class Payload {
        final int id;
        final String name;
        final long[] filler;

        Payload(int id) {
            this.id = id;
            this.name = "payload-" + id;
            // Big enough to make the young generation turn over quickly, small
            // enough to stay a young object rather than a humongous allocation.
            this.filler = new long[16];
            this.filler[0] = id;
            this.filler[15] = ~id;
        }

        /** The virtual call a stale receiver cannot dispatch. */
        int identity() {
            return id;
        }
    }

    static void check(Payload p, int expect, int depth, String slot) {
        checked++;
        String failure = null;
        if (p == null) {
            failure = "null";
        } else {
            try {
                if (p.id != expect) {
                    failure = "field id=" + p.id;
                } else if (p.identity() != expect) {
                    failure = "virtual identity()=" + p.identity();
                } else if (!("payload-" + expect).equals(p.name)) {
                    failure = "field name=" + p.name;
                } else if (p.filler.length != 16 || p.filler[0] != expect || p.filler[15] != ~expect) {
                    failure = "array filler corrupted";
                }
            } catch (Throwable t) {
                // A reclaimed receiver reaches bytecode as `java.lang.Object`:
                // the field reads raise NoSuchFieldError and the virtual call
                // raises NoSuchMethodError. Both are the defect, not the test.
                failure = t.getClass().getName() + ": " + t.getMessage();
            }
        }
        if (failure != null) {
            corrupted++;
            System.out.println("CORRUPT depth=" + depth + " " + slot + " expect=" + expect
                    + " -> " + failure);
        }
    }

    static String churn() {
        // Allocate enough per task that a full round turns the young
        // generation over several times.
        StringBuilder sb = new StringBuilder(64);
        Object[] keep = new Object[8];
        for (int i = 0; i < CHURN; i++) {
            keep[i & 7] = new long[8];
            if ((i & 255) == 0) {
                sb.append(i);
            }
        }
        return sb.length() > 0 ? sb.substring(0, 1) : "";
    }

    static void level(int depth, ExecutorService pool) throws Exception {
        int base = depth * 4;
        Payload a = new Payload(base);
        Payload b = new Payload(base + 1);
        Payload c = new Payload(base + 2);
        Payload d = new Payload(base + 3);

        if (depth > 0) {
            level(depth - 1, pool);
        } else {
            for (int round = 0; round < ROUNDS; round++) {
                List<Callable<String>> tasks = new ArrayList<>(TASKS);
                for (int t = 0; t < TASKS; t++) {
                    tasks.add(BlockedFrameRootProbe::churn);
                }
                // The blocking call. The main thread deposits its root
                // snapshot, raises `in_blocked_region`, and sleeps through
                // every collection the pool threads trigger.
                pool.invokeAll(tasks);
            }
        }

        // Read every local back AFTER the blocked window. These four uses are
        // what keeps the slots live in the per-bci liveness mask, so a
        // collector that drops them dropped a root it was required to keep.
        check(a, base, depth, "local-a");
        check(b, base + 1, depth, "local-b");
        check(c, base + 2, depth, "local-c");
        check(d, base + 3, depth, "local-d");
    }

    public static void main(String[] args) throws Exception {
        System.out.println("BlockedFrameRootProbe depth=" + DEPTH + " passes=" + PASSES
                + " rounds=" + ROUNDS + " tasks=" + TASKS + " pool=" + POOL + " churn=" + CHURN);
        long t0 = System.nanoTime();
        ExecutorService pool = Executors.newFixedThreadPool(POOL);
        try {
            // Each pass rebuilds the deep stack, blocks through a fresh set of
            // collections, and re-verifies every slot: one shot per run would
            // need the rare cycle to land inside a single window.
            for (int pass = 0; pass < PASSES; pass++) {
                int before = corrupted;
                level(DEPTH, pool);
                if (corrupted != before) {
                    System.out.println("  (pass " + pass + " added " + (corrupted - before)
                            + " corrupted slots)");
                }
            }
        } finally {
            pool.shutdown();
            pool.awaitTermination(30, TimeUnit.SECONDS);
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        System.out.println("@@PROBE checked=" + checked + " corrupted=" + corrupted + " ms=" + ms);
        if (corrupted != 0) {
            System.exit(1);
        }
    }
}
