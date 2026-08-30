import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Does `ConcurrentHashMap.newKeySet()` keep every element added concurrently?
 *
 * WHY THIS EXISTS. `org.h2.test.jdbc.TestCachedQueryResults` fails on CratonVM
 * with `Expected: 100000 actual: 98304`, in BOTH modes, and the page that owns
 * that class frames it as a ZGC OOM livelock. The number says otherwise:
 *
 *     98304 == 131072 - (131072 >>> 2)
 *
 * which is exactly ConcurrentHashMap's RESIZE THRESHOLD for a 131072-bucket
 * table. A set that stops at its own resize threshold has not been evicted by
 * memory pressure; it has failed to grow. Random OOM eviction also does not
 * land on the same number twice, and this one did.
 *
 * So this probe strips H2, JDBC, threads-pools and the database out and asks
 * the one question directly: N threads, each adding a disjoint block of
 * distinct Integers to one keySet, then compare `size()` against how many adds
 * reported success.
 *
 * It prints THREE numbers, not one, because they distinguish the possible
 * causes:
 *   * `adds-returned-true` -- how many calls claimed to insert. If this is
 *     100000 and `size` is short, the entries were accepted and then LOST.
 *   * `size` -- what the set says it holds.
 *   * `contains-misses` -- how many of the keys we added cannot be found
 *     afterwards, which is the same question asked through the read path
 *     rather than the counter, in case `size()` alone is the thing that is
 *     wrong.
 *
 * A `size` that is right while `contains` misses (or vice versa) is a different
 * defect from both being short, and one number could not tell them apart.
 */
public final class ChmKeySetGrowth {
    public static void main(String[] args) throws Exception {
        int threads = args.length > 0 ? Integer.parseInt(args[0]) : 5;
        int total   = args.length > 1 ? Integer.parseInt(args[1]) : 100_000;
        int perThread = total / threads;

        Set<Integer> set = ConcurrentHashMap.newKeySet();
        AtomicInteger addedTrue = new AtomicInteger();
        CountDownLatch start = new CountDownLatch(1);
        CountDownLatch done = new CountDownLatch(threads);
        Thread[] ts = new Thread[threads];

        for (int t = 0; t < threads; t++) {
            final int base = t * perThread;
            ts[t] = new Thread(() -> {
                try {
                    start.await();
                    for (int i = 0; i < perThread; i++) {
                        if (set.add(base + i)) {
                            addedTrue.incrementAndGet();
                        }
                    }
                } catch (InterruptedException e) {
                    Thread.currentThread().interrupt();
                } finally {
                    done.countDown();
                }
            });
            ts[t].start();
        }
        start.countDown();
        done.await();

        int misses = 0;
        for (int i = 0; i < perThread * threads; i++) {
            if (!set.contains(i)) {
                misses++;
            }
        }

        System.out.println("threads             |" + threads + "|");
        System.out.println("intended            |" + (perThread * threads) + "|");
        System.out.println("adds-returned-true  |" + addedTrue.get() + "|");
        System.out.println("size                |" + set.size() + "|");
        System.out.println("contains-misses     |" + misses + "|");
        System.out.println("size-matches        |" + (set.size() == perThread * threads) + "|");
    }
}
