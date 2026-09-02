import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.SynchronousQueue;
import java.util.concurrent.TimeUnit;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Which side of a SynchronousQueue handoff is being lost?
 *
 * `JdkOnlyPlatformProbe`'s `handoffs=` row counts only SUCCESSFUL polls, and
 * ignores what `offer` returned -- so a shortfall names no side. On HotSpot the
 * row is 64 every time; on CratonVM it is 64 on an idle host and 55-58 on a
 * loaded one. A count that is deterministic on the oracle and not on the VM is
 * a defect, not an unusable probe row, and the first thing it needs is an
 * attribution.
 *
 * Both halves are counted here, plus the timeout each side was given, because
 * "the handoff was lost" and "20 seconds was not enough on a loaded host" are
 * different findings and the original row cannot tell them apart.
 *
 * Rows are printed as counts, never as timings, so the output is diffable
 * against HotSpot.
 */
public class VtHandoffProbe {

    static int rows = 0;

    static void row(String k, Object v) {
        System.out.println((++rows) + " " + k + " |" + v + "|");
    }

    /** One round: N virtual threads polling, one platform thread offering. */
    static int[] round(int n, long timeoutMs) throws Exception {
        SynchronousQueue<Integer> sq = new SynchronousQueue<>();
        AtomicInteger got = new AtomicInteger();      // poll returned a value
        AtomicInteger nullPoll = new AtomicInteger(); // poll timed out
        AtomicInteger interrupted = new AtomicInteger();
        List<Thread> vs = new ArrayList<>();
        for (int i = 0; i < n; i++) {
            vs.add(Thread.startVirtualThread(() -> {
                try {
                    Integer v = sq.poll(timeoutMs, TimeUnit.MILLISECONDS);
                    if (v != null) {
                        got.incrementAndGet();
                    } else {
                        nullPoll.incrementAndGet();
                    }
                } catch (InterruptedException e) {
                    interrupted.incrementAndGet();
                }
            }));
        }
        int offered = 0, refused = 0;
        for (int i = 0; i < n; i++) {
            if (sq.offer(i, timeoutMs, TimeUnit.MILLISECONDS)) {
                offered++;
            } else {
                refused++;
            }
        }
        int joined = 0;
        for (Thread t : vs) {
            if (t.join(java.time.Duration.ofMillis(timeoutMs))) {
                joined++;
            }
        }
        return new int[] { got.get(), nullPoll.get(), interrupted.get(), offered, refused, joined };
    }

    public static void main(String[] args) throws Exception {
        final int N = 64;
        final long T = 20_000;
        final int ROUNDS = 8;

        // Aggregate over rounds rather than printing per-round numbers: a
        // per-round row would itself be nondeterministic on a VM that drops
        // handoffs, which is the thing under test.
        int okAll = 0, nullAll = 0, intrAll = 0, offAll = 0, refAll = 0, joinAll = 0;
        int roundsFullyMatched = 0;
        for (int r = 0; r < ROUNDS; r++) {
            int[] c = round(N, T);
            okAll += c[0]; nullAll += c[1]; intrAll += c[2];
            offAll += c[3]; refAll += c[4]; joinAll += c[5];
            if (c[0] == N && c[3] == N) roundsFullyMatched++;
        }

        int expected = N * ROUNDS;
        row("rounds", ROUNDS);
        row("threads per round", N);
        row("rounds where every offer AND every poll matched", roundsFullyMatched);
        row("polls that received a value", okAll);
        row("polls that timed out (null)", nullAll);
        row("polls interrupted", intrAll);
        row("offers accepted by a taker", offAll);
        row("offers that timed out (refused)", refAll);
        row("threads joined within the timeout", joinAll);
        row("every poll received a value", okAll == expected);
        row("every offer found a taker", offAll == expected);
        row("accounted for (got + null + interrupted)", okAll + nullAll + intrAll == expected);
        row("offers and successful polls agree", okAll == offAll);
        row("every thread joined", joinAll == expected);

        System.out.println("ROWS " + rows);
        System.out.println("DONE VtHandoffProbe");
    }
}
