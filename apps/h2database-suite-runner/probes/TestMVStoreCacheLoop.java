package org.h2.test.store;

import java.util.Random;
import java.util.concurrent.atomic.AtomicInteger;

import org.h2.mvstore.MVMap;
import org.h2.mvstore.MVStore;
import org.h2.store.fs.FileUtils;
import org.h2.test.TestBase;
import org.h2.util.Task;

/**
 * Byte-for-byte the body of {@link TestMVStoreCachePerformance}, except that
 * after the two single-threaded warm-up rounds it LOOPS the two 10-thread
 * rounds instead of moving on to the 100-thread ones.
 *
 * Rounds 3 and 4 are where
 * the retired bug-h2-classid0-stale-address-family write-up
 * reproduces; the stock class reaches them once, ~730 s in, and then leaves the
 * regime for good. Looping them keeps the heap state (a store closed and
 * rebuilt each round, old gen carrying the previous rounds' debris) and the
 * concurrency identical while multiplying the exposure per run.
 */
public class TestMVStoreCacheLoop extends TestBase {

    public static void main(String... a) throws Exception {
        TestBase test = TestBase.createCaller().init();
        test.test();
    }

    @Override
    public void test() throws Exception {
        testCache(1, "");
        testCache(1, "cache:");
        int round = 0;
        while (true) {
            round++;
            testCache(10, "");
            testCache(10, "cache:");
            System.out.println("=== loop round " + round + " complete");
            System.out.flush();
        }
    }

    private void testCache(int threadCount, String fileNamePrefix) {
        String fileName = getBaseDir() + "/" + getTestName();
        fileName = fileNamePrefix  + fileName;
        FileUtils.delete(fileName);
        MVStore store = new MVStore.Builder().
                fileName(fileName).
                open();
        final MVMap<Integer, byte[]> map = store.openMap("test");
        final AtomicInteger counter = new AtomicInteger();
        byte[] data = new byte[8 * 1024];
        final int count = 10000;
        for (int i = 0; i < count; i++) {
            map.put(i, data);
            store.commit();
        }
        Task[] tasks = new Task[threadCount];
        for (int i = 0; i < threadCount; i++) {
            tasks[i] = new Task() {

                @Override
                public void call() throws Exception {
                    Random r = new Random();
                    do {
                        int id = r.nextInt(count);
                        map.get(id);
                        counter.incrementAndGet();
                    } while (!stop);
                }

            };
            tasks[i].execute();
        }
        for (int i = 0; i < 4; i++) {
            try {
                Thread.sleep(1000);
            } catch (InterruptedException e) {
                // ignore
            }
        }
        for (Task t : tasks) {
            t.get();
        }
        store.close();
        System.out.println(counter.get() / 10000 + " ops/ms; " +
                threadCount + " thread(s); " + fileNamePrefix);
        System.out.flush();
    }

}
