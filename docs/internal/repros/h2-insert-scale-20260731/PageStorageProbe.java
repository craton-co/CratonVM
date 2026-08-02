import java.nio.ByteBuffer;
import java.util.Random;
import java.util.concurrent.atomic.AtomicLong;

import org.h2.mvstore.WriteBuffer;
import org.h2.mvstore.type.BasicDataType;
import org.h2.util.MemoryEstimator;

/**
 * Witness for the NullPointerException CratonVM raises inside
 * TestMemoryEstimator.testPageEstimator.
 *
 * That loop is:
 *     Integer[] storage = dataType.createStorage(pageSz);   // generic T[] via a bridge
 *     for (int k = 0; k < pageSz; k++) {
 *         storage[k] = (int) Math.abs(100 + random.nextGaussian() * 30);  // box + aastore
 *         x += storage[k];                                                // aaload + unbox
 *     }
 *
 * so an NPE there means an element read back null immediately after being
 * stored. Seeded so a failure is reproducible, and each stage is checked
 * separately (`storeReadBack`, `unboxSum`) so the log says WHICH read failed.
 *
 * Usage: PageStorageProbe [rounds] [seed]
 */
public class PageStorageProbe {

    private static class TestDataType extends BasicDataType<Integer> {
        @Override
        public int getMemory(Integer obj) {
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

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 20000;
        long seed = args.length > 1 ? Long.parseLong(args[1]) : 42L;
        Random random = new Random(seed);
        TestDataType dataType = new TestDataType();
        AtomicLong stat = new AtomicLong();

        long nullAfterStore = 0;
        long nullAtSum = 0;
        long shortArray = 0;
        long checksum = 0;
        long estimates = 0;

        for (int i = 0; i < rounds; i++) {
            int pageSz = random.nextInt(48) + 1;
            Integer[] storage = dataType.createStorage(pageSz);
            if (storage == null || storage.length != pageSz) {
                shortArray++;
                continue;
            }
            int x = 0;
            for (int k = 0; k < pageSz; k++) {
                int v = (int) Math.abs(100 + random.nextGaussian() * 30);
                storage[k] = v;
                // Stage 1: does the slot read back at all?
                Integer back = storage[k];
                if (back == null) {
                    nullAfterStore++;
                    if (nullAfterStore == 1) {
                        System.out.println("first null-after-store: round=" + i + " k=" + k
                                + " pageSz=" + pageSz + " len=" + storage.length + " v=" + v);
                    }
                    storage[k] = v;
                    back = storage[k];
                    if (back == null) {
                        System.out.println("  still null after re-store");
                        continue;
                    }
                }
                // Stage 2: the unboxing read the real test performs.
                try {
                    x += storage[k];
                } catch (NullPointerException e) {
                    nullAtSum++;
                    if (nullAtSum == 1) {
                        System.out.println("first null-at-sum: round=" + i + " k=" + k
                                + " pageSz=" + pageSz + " len=" + storage.length);
                    }
                }
            }
            checksum = checksum * 31 + x;
            // Stage 3: the same array through the real H2 estimator overload.
            estimates += MemoryEstimator.estimateMemory(stat, dataType, storage, pageSz);
        }

        System.out.println("rounds=" + rounds + " seed=" + seed);
        System.out.println("shortArray=" + shortArray);
        System.out.println("nullAfterStore=" + nullAfterStore);
        System.out.println("nullAtSum=" + nullAtSum);
        System.out.println("checksum=" + checksum);
        System.out.println("estimates=" + estimates);
        System.out.println(shortArray == 0 && nullAfterStore == 0 && nullAtSum == 0 ? "PASS" : "FAIL");
    }
}
