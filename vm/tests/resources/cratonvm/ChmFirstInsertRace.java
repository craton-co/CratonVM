package cratonvm;

import java.util.concurrent.ConcurrentHashMap;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicInteger;

/**
 * Fixture for {@code vm/tests/chm_first_insert_race.rs}: several threads make
 * the FIRST inserts into one fresh {@code ConcurrentHashMap} at once.
 *
 * <p>The default constructor defers the map's storage to its first insert, and
 * that install used to be a plain store: two first inserts each built storage,
 * the later one won, and the earlier one's key -- already reserved in the losing
 * storage -- was lost, with {@code computeIfAbsent} returning null. This is the
 * shape of Tomcat's {@code TimeBucketCounterBase.increment}, which threw
 * NullPointerException on a client's first request.
 *
 * <p>Compile: {@code javac -d vm/tests/resources
 * vm/tests/resources/cratonvm/ChmFirstInsertRace.java}
 */
public final class ChmFirstInsertRace {

    /**
     * {@code rounds} fresh maps, {@code threads} threads each doing
     * {@code computeIfAbsent} on its own key, released together. Answers the
     * number of rounds that went wrong: a null return, or a map left with fewer
     * than {@code threads} entries. The JDK answers 0.
     */
    public static int badRounds(int rounds, int threads) throws InterruptedException {
        int bad = 0;
        for (int r = 0; r < rounds; r++) {
            final ConcurrentHashMap<String, AtomicInteger> map = new ConcurrentHashMap<>();
            final CountDownLatch go = new CountDownLatch(1);
            final AtomicInteger nulls = new AtomicInteger();
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final String key = "k" + t;
                ts[t] = new Thread(new Runnable() {
                    @Override
                    public void run() {
                        try {
                            go.await();
                        } catch (InterruptedException e) {
                            return;
                        }
                        AtomicInteger ai = map.computeIfAbsent(key, ChmFirstInsertRace::newCounter);
                        if (ai == null) {
                            nulls.incrementAndGet();
                        } else {
                            ai.incrementAndGet();
                        }
                    }
                });
                ts[t].start();
            }
            go.countDown();
            for (Thread t : ts) {
                t.join();
            }
            if (nulls.get() != 0 || map.size() != threads) {
                bad++;
            }
        }
        return bad;
    }

    private static AtomicInteger newCounter(String ignored) {
        return new AtomicInteger();
    }

    /** Same race through {@code put}: rounds whose map lost an entry. */
    public static int putBadRounds(int rounds, int threads) throws InterruptedException {
        int bad = 0;
        for (int r = 0; r < rounds; r++) {
            final ConcurrentHashMap<String, Integer> map = new ConcurrentHashMap<>();
            final CountDownLatch go = new CountDownLatch(1);
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final String key = "k" + t;
                final Integer value = Integer.valueOf(t);
                ts[t] = new Thread(new Runnable() {
                    @Override
                    public void run() {
                        try {
                            go.await();
                        } catch (InterruptedException e) {
                            return;
                        }
                        map.put(key, value);
                    }
                });
                ts[t].start();
            }
            go.countDown();
            for (Thread t : ts) {
                t.join();
            }
            if (map.size() != threads) {
                bad++;
            }
        }
        return bad;
    }

    public static void main(String[] args) throws InterruptedException {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 2000;
        long t0 = System.nanoTime();
        int cia = badRounds(rounds, 4);
        int put = putBadRounds(rounds, 4);
        System.out.println("computeIfAbsent bad rounds=" + cia + "/" + rounds + ", put bad rounds=" + put + "/"
                + rounds + " (" + (System.nanoTime() - t0) / 1_000_000 + " ms)");
    }
}
