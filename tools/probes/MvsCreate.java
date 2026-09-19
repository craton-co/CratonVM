import org.h2.mvstore.MVMap;
import org.h2.mvstore.MVStore;
import org.h2.mvstore.rtree.MVRTreeMap;
import org.h2.mvstore.db.SpatialKey;
import org.h2.store.fs.FileUtils;

import java.util.Random;

/**
 * The CREATE phase of {@code org.h2.test.store.TestMVStoreTool.testCompact()},
 * lifted out with the entry count as {@code argv[0]} and each sub-phase timed
 * separately.
 *
 * <h2>Why this exists</h2>
 *
 * The class itself hard-codes {@code config.big = true} in its {@code main},
 * i.e. 2,000,000 entries, which HotSpot writes in 5.7 s and CratonVM does not
 * finish inside 30 minutes. That makes the workload unmeasurable: every arm is
 * a TIMEOUT, and a timeout carries no number to compare. At {@code N=100000}
 * the same shape is 0.45 s on HotSpot and ~13 s on CratonVM -- the same ratio,
 * priced in seconds -- so a change can be A/B'd in one minute instead of one
 * hour. Two of the class's four documented failures (the ZGC
 * {@code OutOfMemoryError ... native reference array} and the throughput wall)
 * reproduce here.
 *
 * <pre>
 * javac -cp "$H2CP" probes/MvsCreate.java -d /tmp/p
 * java   -Xmx256m -cp "/tmp/p:$H2CP" MvsCreate 100000 ./data/m
 * cratonvm --java-home "$JDK" --Xmx 256m -c "/tmp/p:$H2CP" MvsCreate 100000 ./data/m
 * </pre>
 *
 * {@code argv[1]} is the store directory, so concurrent A/B arms do not share
 * one file. See
 * {@code docs/internal/performance/h2-mvstoretool-create-phase-is-address-validation-20260907.md}.
 */
public class MvsCreate {
    public static void main(String[] a) throws Exception {
        int size = a.length > 0 ? Integer.parseInt(a[0]) : 100_000;
        String dir = a.length > 1 ? a[1] : "./data/mvscreate";
        String fileName = dir + "/testCompact.h3";
        FileUtils.createDirectories(dir);
        FileUtils.delete(fileName);

        MVStore s = new MVStore.Builder().
                pageSplitSize(1000).
                fileName(fileName).autoCommitDisabled().open();
        s.setRetentionTime(0);

        long t0 = System.currentTimeMillis();
        MVMap<Integer, String> map = s.openMap("data");
        for (int i = 0; i < size; i++) {
            map.put(i, "Hello World " + i * 10);
            if (i % 10000 == 0) {
                s.commit();
            }
        }
        long t1 = System.currentTimeMillis();
        System.out.println("PHASE put      " + (t1 - t0) + " ms");

        for (int i = 0; i < size; i += 2) {
            map.remove(i);
            if (i % 10000 == 0) {
                s.commit();
            }
        }
        long t2 = System.currentTimeMillis();
        System.out.println("PHASE remove   " + (t2 - t1) + " ms");

        for (int i = 0; i < 20; i++) {
            map = s.openMap("data" + i);
            for (int j = 0; j < i * i; j++) {
                map.put(j, "Hello World " + j * 10);
            }
            s.commit();
        }
        long t3 = System.currentTimeMillis();
        System.out.println("PHASE maps     " + (t3 - t2) + " ms");

        MVRTreeMap<String> rTreeMap = s.openMap("rtree", new MVRTreeMap.Builder<>());
        Random r = new Random(1);
        for (int i = 0; i < 10; i++) {
            float x = r.nextFloat();
            float y = r.nextFloat();
            float width = r.nextFloat() / 10;
            float height = r.nextFloat() / 10;
            SpatialKey k = new SpatialKey(i, x, x + width, y, y + height);
            rTreeMap.put(k, "Hello World " + i * 10);
            if (i % 3 == 0) {
                s.commit();
            }
        }
        s.close();
        long t4 = System.currentTimeMillis();
        System.out.println("PHASE rtree    " + (t4 - t3) + " ms");
        System.out.println("TOTAL create   " + (t4 - t0) + " ms  size=" + size);
        System.out.println("DONE");
    }
}
