import java.util.Random;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

import org.h2.mvstore.MVMap;
import org.h2.mvstore.MVStore;
import org.h2.store.fs.FileUtils;

/**
 * Accelerated stand-in for org.h2.test.store.TestMVStoreCachePerformance's
 * round 3/4 (`testCache(10, ...)`), which is where
 * the retired bug-h2-mvstore-readpagefromcache-classid0-nonmoving-sweep write-up
 * reproduces. The stock test spends ~700 s in two single-threaded rounds
 * before it ever starts the 10-reader phase, and then runs that phase for
 * only 4 s. This does the population once and then runs the 10-reader phase
 * continuously, so the regime under test is live essentially 100 % of the
 * wall clock instead of ~1 %.
 *
 * args: [threads] [seconds] [entries]
 */
public class MVCacheStress {

    public static void main(String... a) throws Exception {
        int threads = a.length > 0 ? Integer.parseInt(a[0]) : 10;
        int seconds = a.length > 1 ? Integer.parseInt(a[1]) : 3600;
        final int count = a.length > 2 ? Integer.parseInt(a[2]) : 10000;
        String fileName = "./data/mvcachestress";
        FileUtils.createDirectories("./data");
        FileUtils.delete(fileName);
        MVStore store = new MVStore.Builder().fileName(fileName).open();
        final MVMap<Integer, byte[]> map = store.openMap("test");
        byte[] data = new byte[8 * 1024];
        for (int i = 0; i < count; i++) {
            map.put(i, data);
            store.commit();
        }
        System.out.println("populated " + count + " entries");
        System.out.flush();

        final AtomicLong counter = new AtomicLong();
        final AtomicInteger failures = new AtomicInteger();
        final long deadline = System.currentTimeMillis() + seconds * 1000L;
        Thread[] ts = new Thread[threads];
        for (int i = 0; i < threads; i++) {
            ts[i] = new Thread(new Runnable() {
                @Override
                public void run() {
                    Random r = new Random();
                    try {
                        while (System.currentTimeMillis() < deadline) {
                            for (int k = 0; k < 1000; k++) {
                                map.get(r.nextInt(count));
                            }
                            counter.addAndGet(1000);
                        }
                    } catch (Throwable t) {
                        failures.incrementAndGet();
                        System.out.println("READER FAILED: " + t);
                        t.printStackTrace(System.out);
                        System.out.flush();
                    }
                }
            });
            ts[i].setDaemon(true);
            ts[i].start();
        }
        long lastReport = System.currentTimeMillis();
        while (System.currentTimeMillis() < deadline && failures.get() == 0) {
            Thread.sleep(500);
            long now = System.currentTimeMillis();
            if (now - lastReport >= 30000) {
                lastReport = now;
                System.out.println("ops=" + counter.get());
                System.out.flush();
            }
        }
        System.out.println("done ops=" + counter.get() + " failures=" + failures.get());
        System.out.flush();
        if (failures.get() != 0) {
            Runtime.getRuntime().halt(3);
        }
        store.close();
    }
}
