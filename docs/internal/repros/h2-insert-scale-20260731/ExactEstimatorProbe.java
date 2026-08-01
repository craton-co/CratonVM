import java.nio.ByteBuffer;
import java.util.Random;
import java.util.concurrent.atomic.AtomicLong;

import org.h2.mvstore.WriteBuffer;
import org.h2.mvstore.type.BasicDataType;
import org.h2.util.MemoryEstimator;

/**
 * Byte-for-byte copy of org.h2.test.unit.TestMemoryEstimator's two methods,
 * with the assertions removed and a real Java stack trace printed on failure.
 *
 * WHY: CratonVM NPEs in testPageEstimator (identically pre- and post-fix,
 * while HotSpot passes), but PageStorageProbe — which mirrors that inner loop —
 * runs 20000 rounds clean. So the trigger is something the reduced probe drops.
 * The two differences are (a) testEstimator() runs FIRST in the real test, and
 * (b) the real test uses an UNSEEDED Random. This keeps both, and prints the
 * JDK's own stack trace so the failing line is authoritative rather than
 * CratonVM's frame attribution.
 *
 * Usage: ExactEstimatorProbe [rounds]
 */
public class ExactEstimatorProbe {

    private static class TestDataType extends BasicDataType<Integer> {
        private int count;

        public int getCount() {
            return count;
        }

        @Override
        public int getMemory(Integer obj) {
            ++count;
            return obj;
        }

        @Override
        public void write(WriteBuffer buff, Integer obj) {}

        @Override
        public Integer read(ByteBuffer buff) {
            return null;
        }

        @Override
        public Integer[] createStorage(int size) {
            return new Integer[size];
        }
    }

    private static void testEstimator() {
        Random random = new Random();
        AtomicLong stat = new AtomicLong();
        TestDataType dataType = new TestDataType();
        int sum = 0, sum2 = 0, err2 = 0;
        int size = 10000;
        for (int i = 0; i < size; i++) {
            int x = (int) Math.abs(100 + random.nextGaussian() * 30);
            int y = MemoryEstimator.estimateMemory(stat, dataType, x);
            sum += x;
            sum2 += x * x;
            err2 += (x - y) * (x - y);
        }
        int avg = sum / size;
        double err = Math.sqrt(1.0 * err2 / sum2);
        int pct = MemoryEstimator.samplingPct(stat);
        System.out.println("testEstimator     Avg=" + avg + ", err=" + err + ", pct=" + pct
                + " " + (dataType.getCount() * 100 / size)
                + "   [bounds: err<0.3 pct<=7]");
    }

    private static void testPageEstimator() {
        Random random = new Random();
        AtomicLong stat = new AtomicLong();
        TestDataType dataType = new TestDataType();
        long sum = 0, sum2 = 0, err2 = 0;
        int size = 10000;
        int pageSz;
        for (int i = 0; i < size; i += pageSz) {
            pageSz = random.nextInt(48) + 1;
            Integer[] storage = dataType.createStorage(pageSz);
            int x = 0;
            for (int k = 0; k < pageSz; k++) {
                storage[k] = (int) Math.abs(100 + random.nextGaussian() * 30);
                x += storage[k];
            }
            int y = MemoryEstimator.estimateMemory(stat, dataType, storage, pageSz);
            sum += x;
            sum2 += (long) x * x;
            err2 += (long) (x - y) * (x - y);
        }
        long avg = sum / size;
        double err = Math.sqrt(1.0 * err2 / sum2);
        int pct = MemoryEstimator.samplingPct(stat);
        System.out.println("testPageEstimator Avg=" + avg + ", err=" + err + ", pct=" + pct
                + " " + (dataType.getCount() * 100 / size)
                + "   [bounds: err<0.12 pct<=4]");
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 1;
        for (int r = 0; r < rounds; r++) {
            System.out.println("=== round " + r + " ===");
            try {
                testEstimator();
            } catch (Throwable t) {
                System.out.println("testEstimator THREW:");
                t.printStackTrace(System.out);
            }
            try {
                testPageEstimator();
            } catch (Throwable t) {
                System.out.println("testPageEstimator THREW:");
                t.printStackTrace(System.out);
            }
        }
        System.out.println("DONE");
    }
}
