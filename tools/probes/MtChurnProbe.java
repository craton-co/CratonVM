/**
 * Multi-threaded allocation churn with a GROWING retained set.
 *
 * <p>The two shapes the cross-collector page's "what a soak should measure"
 * list names and which nothing in {@code probes/} exercised:
 *
 * <ul>
 *   <li><b>A multi-threaded allocator.</b> Every young-GC trigger consult and
 *       every TLAB refill lands on N threads at once, so a shared line taken
 *       exclusive on that path costs N times what a single-threaded probe can
 *       show. That is the population {@code CRATONVM_GC_TRIGGER_LOCKFREE}
 *       exists for, and a single-threaded probe measures an uncontended lock.
 *   <li><b>Young arenas that GROW.</b> The retained set climbs across rounds
 *       rather than staying fixed, so the heap expands under load instead of
 *       settling at its initial split — the path the exact object-start bitmap
 *       is rebuilt on, and the one nothing had run.
 * </ul>
 *
 * <p>Prints one {@code MTCHURN_OK} line carrying a checksum, so an arm that
 * failed to start, threw, or ran a different amount of work is not silently
 * timed as if it had. A run that does not print it is not a reading.
 *
 * <p>Usage: {@code MtChurnProbe <threads> <rounds> <growMiB>}
 */
public final class MtChurnProbe {

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

    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 4;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 60;
        int growMiB = args.length > 2 ? Integer.parseInt(args[2]) : 48;

        final int nodeBytes = 256;
        // Retained nodes each thread adds per round. Summed over rounds this is
        // the growth: the live set is not fixed, so the young arena has to
        // expand rather than settle.
        final int growPerRound =
                Math.max(1, (growMiB * 1024 * 1024) / (nodeBytes * threads * rounds));

        final long[] sums = new long[threads];
        Thread[] ts = new Thread[threads];
        long start = System.nanoTime();

        for (int t = 0; t < threads; t++) {
            final int id = t;
            ts[t] = new Thread(() -> {
                Node retained = null;
                long sum = 0;
                for (int r = 0; r < rounds; r++) {
                    // The garbage stream: 1 MiB per thread per round, dead
                    // before the next pause.
                    for (int i = 0; i < (1024 * 1024) / nodeBytes; i++) {
                        Node dead = new Node(null, nodeBytes - 32, i);
                        sum += dead.payload.length + dead.tag;
                    }
                    // ...and the growth.
                    for (int i = 0; i < growPerRound; i++) {
                        retained = new Node(retained, nodeBytes - 32, r);
                    }
                    // Touch the retained chain so it is genuinely live and the
                    // collector has to copy or promote it.
                    for (Node n = retained; n != null; n = n.next) {
                        sum += n.tag;
                    }
                }
                sums[id] = sum;
            }, "mtchurn-" + id);
            ts[t].start();
        }
        for (Thread th : ts) {
            th.join();
        }

        long checksum = 0;
        for (long s : sums) {
            checksum += s;
        }
        long wallMs = (System.nanoTime() - start) / 1_000_000L;
        System.out.println("MTCHURN_OK threads=" + threads + " rounds=" + rounds
                + " growMiB=" + growMiB + " wallMs=" + wallMs
                + " checksum=" + checksum);
    }
}
