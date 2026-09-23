/**
 * {@code G1OldBurstProbe}'s grow/settle shape, run on N MUTATOR THREADS — the
 * "thread-heavy arm" wave 3 and wave 5 both asked for and neither ran.
 *
 * <p>WHY THIS EXISTS. {@code CRATONVM_G1_IHOP_BACKOFF} has been measured twice,
 * both times on a single-threaded workload, and both pages closed on the same
 * unpriced cost:
 *
 * <ul>
 *   <li>wave 3 ({@code w3c-the-six-w2c-flags-measured.md} §3):
 *       {@code ihop_polls} 18 → 1 616 077 with the back-off armed;
 *   <li>wave 5 ({@code w5c-the-back-off-on-a-heap-that-grows.md} §6):
 *       {@code ihop_polls} 348 → 41 641 213, of which
 *       {@code backoff_declined_polls} 41 640 949, against 130 mark cycles.
 * </ul>
 *
 * <p>The mechanism is structural rather than a tuning number. With the back-off
 * OFF, {@code check_ihop} passes, a cycle starts, and
 * {@code g1_is_marking_active()} short-circuits the JIT mark driver for the
 * cycle's whole duration — so the gate is consulted a few hundred times per
 * run. With it ON the cycle is refused, marking never becomes active, and
 * <b>every subsequent JIT allocation helper re-polls</b>
 * ({@code jit_new_object} / {@code jit_newarray} / {@code jit_anewarray_object}
 * each call {@code jit_drive_g1_concurrent_mark} unconditionally). The poll
 * rate is therefore the ALLOCATION rate, which is the one quantity the back-off
 * cannot bound.
 *
 * <p>WHAT ONLY A THREAD-HEAVY ARM CAN SHOW. {@code check_ihop} does not merely
 * READ on the declined path. It performs {@code IHOP_POLLS.fetch_add(1)} and
 * {@code mark_backoff_suppressions.fetch_add(1)} — two read-modify-writes on
 * two PROCESS-GLOBAL cache lines, per declined poll. Single-threaded those are
 * uncontended and near-free, which is why two batteries reported the poll count
 * as a worry rather than as a cost. With N mutator threads allocating they are
 * N-way contention on two lines at allocation rate, and the line has to make a
 * round trip between cores for every single one. Wave 5 §6 said so and said
 * plainly that its probe had one thread and could not measure it. This probe
 * has N.
 *
 * <p>THE SHAPE is {@code G1OldBurstProbe}'s, per thread, and the reasoning for
 * it is that probe's javadoc and is not repeated here: a grow half in which
 * every mark cycle is genuinely futile (and so widens the back-off's gate),
 * then a settle half in which a large old set is dropped in one store and only
 * young garbage is allocated (so occupancy is flat, a large fraction of it is
 * dead, and a suppressed cycle costs exactly that fraction). A fixed retained
 * set makes the back-off trivially right; a monotonically growing one with no
 * garbage makes it trivially wrong; this is the mixture.
 *
 * <p>WORK IS HELD CONSTANT ACROSS THE THREAD SWEEP. Every MiB argument is a
 * TOTAL over the whole process, divided by {@code threads}. So sweeping
 * {@code threads} 1 → 8 → 32 holds the live set, the total allocation and the
 * heap pressure fixed and varies only how many threads are polling the gate —
 * which is the one variable under test. A probe that gave each thread a fixed
 * quantum would confound contention with heap pressure, and the wall clock
 * would move for the wrong reason.
 *
 * <p>THE CHECKSUM folds every node each thread creates and every node it walks,
 * plus a final fold over that thread's surviving core; the process checksum is
 * the SUM over threads, which is order-independent, so it does not depend on
 * the scheduler. It DOES depend on {@code threads} (each thread's tags are
 * derived from its own id and its own share of the work), so the gate is "one
 * distinct checksum per thread count, identical across arms at that thread
 * count" — which is what an A/B needs, since arms are compared within a thread
 * count. A run that is faster because an object was lost, not copied, or freed
 * while reachable produces a different number and fails rather than scores.
 *
 * <p>Usage:
 * {@code G1PollStormProbe <threads> <coreTotalMiB> <coreDeltaTotalMiB> <burstTotalMiB> <phases> <youngChurnTotalMiB> [stripeNodes]}
 * — defaults {@code 4 16 4 48 8 96 64}. Size so
 * {@code coreTotal + phases*coreDeltaTotal + burstTotal} lands in the 50-70%
 * band of the heap.
 */
public final class G1PollStormProbe {

    /** One node: a header, a ref, a tag and a payload. 256 bytes all in. */
    static final class Node {
        Node next;
        final byte[] payload;
        int tag;

        Node(Node next, int payloadBytes, int tag) {
            this.next = next;
            this.payload = new byte[payloadBytes];
            this.tag = tag;
        }
    }

    /** Nominal cost of one node including header and payload header. */
    static final int NODE_BYTES = 256;

    /** Payload bytes per node, leaving room for the two headers and the fields. */
    static final int PAYLOAD_BYTES = NODE_BYTES - 32;

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int coreTotalMiB = args.length > 1 ? Integer.parseInt(args[1]) : 16;
        int coreDeltaTotalMiB = args.length > 2 ? Integer.parseInt(args[2]) : 4;
        int burstTotalMiB = args.length > 3 ? Integer.parseInt(args[3]) : 48;
        int phases = args.length > 4 ? Integer.parseInt(args[4]) : 8;
        int youngChurnTotalMiB = args.length > 5 ? Integer.parseInt(args[5]) : 96;
        int stripeNodes = args.length > 6 ? Integer.parseInt(args[6]) : 64;
        if (threads < 1) {
            threads = 1;
        }
        if (stripeNodes < 1) {
            stripeNodes = 1;
        }

        // Per-thread shares. Integer division, so the process total is
        // `threads * share` and is REPORTED below rather than assumed: a
        // sweep over `threads` that silently changed the total allocation
        // would be measuring heap pressure, not contention.
        final int coreStartNodes = ((coreTotalMiB * 1024 * 1024) / NODE_BYTES) / threads;
        final int coreDeltaNodes = ((coreDeltaTotalMiB * 1024 * 1024) / NODE_BYTES) / threads;
        final int burstNodes = ((burstTotalMiB * 1024 * 1024) / NODE_BYTES) / threads;
        final int youngChurnNodes = ((youngChurnTotalMiB * 1024 * 1024) / NODE_BYTES) / threads;

        final long[] sums = new long[threads];
        final long[] allocated = new long[threads];
        final long[] liveNodesPer = new long[threads];
        // A worker that DIES must not look like a worker that computed zero.
        //
        // Measured 2026-09-21: one run in eleven at `-Xmx192m` ended with
        // `Exception in thread "pollstorm-0" java.lang.NullPointerException:
        // Cannot read the array length` on a `final byte[]` assigned in a
        // constructor and never written again — the signature of the open
        // use-after-free in
        // `docs/internal/g1-2026-09-20/w5c-a-use-after-free-under-old-generation-turnover.md`.
        // An uncaught exception in a `Thread` prints a stack trace and leaves
        // `join()` returning normally, so without this latch the probe printed
        // `liveNodes=0 nodesAllocated=0 checksum=0` and EXITED ZERO. A harness
        // that gates on the checksum catches that; one that gates on the exit
        // code does not, and house rule 6 says a probe asserts its checksum.
        final java.util.concurrent.atomic.AtomicReference<Throwable> died =
                new java.util.concurrent.atomic.AtomicReference<>();
        final int fPhases = phases;
        final int fStripe = stripeNodes;

        Thread[] ts = new Thread[threads];
        long start = System.nanoTime();

        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                long checksum = 0;
                long nodesAllocated = 0;

                // This thread's permanent set. Per-thread rather than shared so
                // the retained graph does not need a lock and the probe is
                // measuring the collector rather than a monitor.
                Node core = null;
                for (int i = 0; i < coreStartNodes; i++) {
                    core = new Node(core, PAYLOAD_BYTES, i + id * 7919);
                    checksum += core.tag + core.payload.length;
                    nodesAllocated++;
                }

                for (int phase = 0; phase < fPhases; phase++) {
                    // ---- grow half -------------------------------------
                    Node burst = null;
                    int burstMade = 0;
                    int coreMade = 0;
                    int tag = phase * 1_000_003 + id * 7919;
                    while (burstMade < burstNodes || coreMade < coreDeltaNodes) {
                        for (int s = 0; s < fStripe && coreMade < coreDeltaNodes; s++, coreMade++) {
                            core = new Node(core, PAYLOAD_BYTES, tag + coreMade);
                            checksum += core.tag + core.payload.length;
                            nodesAllocated++;
                        }
                        for (int s = 0; s < fStripe && burstMade < burstNodes; s++, burstMade++) {
                            burst = new Node(burst, PAYLOAD_BYTES, tag + burstMade);
                            checksum += burst.tag + burst.payload.length;
                            nodesAllocated++;
                        }
                        for (int i = 0; i < fStripe * 4; i++) {
                            Node dead = new Node(null, PAYLOAD_BYTES, i);
                            checksum += dead.payload.length;
                            nodesAllocated++;
                        }
                    }

                    checksum += walk(core, 8192, phase);
                    checksum += walk(burst, 8192, phase);

                    // ---- settle half -----------------------------------
                    //
                    // THE DROP. This thread's whole burst becomes garbage in
                    // one store, with no compensating allocation. Everything
                    // below retains nothing, so old occupancy stays where the
                    // grow half left it while a large fraction of it is dead —
                    // and that is the window in which a refused mark cycle
                    // costs `burstTotalMiB`.
                    burst = null;

                    for (int i = 0; i < youngChurnNodes; i++) {
                        Node dead = new Node(null, PAYLOAD_BYTES, i);
                        checksum += dead.payload.length;
                        nodesAllocated++;
                    }
                    checksum += walk(core, 8192, phase);
                }

                long liveNodes = 0;
                long liveFold = 0;
                for (Node cur = core; cur != null; cur = cur.next) {
                    liveFold += cur.tag + cur.payload.length;
                    liveNodes++;
                }
                checksum += liveFold + liveNodes;

                sums[id] = checksum;
                allocated[id] = nodesAllocated;
                liveNodesPer[id] = liveNodes;
            }, "pollstorm-" + id);
            ts[t].setUncaughtExceptionHandler((th, e) -> {
                died.compareAndSet(null, e);
                e.printStackTrace();
            });
            ts[t].start();
        }
        for (Thread th : ts) {
            th.join();
        }
        Throwable fatal = died.get();
        if (fatal != null) {
            System.out.println("G1PollStormProbe FAILED threads=" + threads
                    + " reason=" + fatal.getClass().getName() + ": " + fatal.getMessage()
                    + " checksum=INVALID");
            System.exit(70);
        }

        long checksum = 0;
        long nodesAllocated = 0;
        long liveNodes = 0;
        for (int i = 0; i < threads; i++) {
            checksum += sums[i];
            nodesAllocated += allocated[i];
            liveNodes += liveNodesPer[i];
        }
        long wallMs = (System.nanoTime() - start) / 1_000_000L;

        System.out.println("G1PollStormProbe"
                + " threads=" + threads
                + " coreTotal=" + coreTotalMiB + "MiB"
                + " coreDeltaTotal=" + coreDeltaTotalMiB + "MiB"
                + " burstTotal=" + burstTotalMiB + "MiB"
                + " phases=" + phases
                + " youngChurnTotal=" + youngChurnTotalMiB + "MiB"
                + " stripeNodes=" + stripeNodes
                + " liveNodes=" + liveNodes
                + " liveMiB=" + ((liveNodes * NODE_BYTES) / (1024 * 1024))
                + " nodesAllocated=" + nodesAllocated
                + " allocMiB=" + ((nodesAllocated * NODE_BYTES) / (1024 * 1024))
                + " wallMs=" + wallMs
                + " checksum=" + checksum);
    }

    /**
     * Walk up to {@code limit} nodes of a chain, mutating each tag so the write
     * barrier runs on an old receiver, and folding the result into a return
     * value the caller adds to the checksum. Bounded so the walk cost does not
     * grow with the core and turn this into a pointer-chasing benchmark.
     */
    static long walk(Node head, int limit, int phase) {
        long fold = 0;
        int walked = 0;
        Node cur = head;
        while (cur != null && walked < limit) {
            cur.tag = cur.tag + phase;
            fold += cur.tag;
            cur = cur.next;
            walked++;
        }
        return fold;
    }
}
