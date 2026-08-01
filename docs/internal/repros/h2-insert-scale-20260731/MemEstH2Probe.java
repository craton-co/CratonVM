import java.nio.ByteBuffer;
import java.util.concurrent.atomic.AtomicLong;

import org.h2.mvstore.WriteBuffer;
import org.h2.mvstore.type.BasicDataType;
import org.h2.util.MemoryEstimator;

/**
 * Deterministic driver for the REAL org.h2.util.MemoryEstimator.
 *
 * H2's own TestMemoryEstimator uses an unseeded Random and asserts a
 * distribution bound (`samplingPct <= 7`), so a JIT divergence shows up only as
 * a marginal statistic. This drives the identical code with a fixed-seed
 * sequence and prints an exact checksum of every returned estimate, turning
 * "the average looks a bit off" into a diff.
 *
 * Usage: MemEstH2Probe [iterations]
 */
public class MemEstH2Probe {

    private static class TestDataType extends BasicDataType<Integer> {
        private int count;

        int getCount() {
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

    public static void main(String[] args) {
        int size = args.length > 0 ? Integer.parseInt(args[0]) : 10000;
        AtomicLong stat = new AtomicLong();
        TestDataType dataType = new TestDataType();
        long seed = 0x5DEECE66DL;
        long sum = 0;
        long sum2 = 0;
        long err2 = 0;
        long checksum = 0;
        for (int i = 0; i < size; i++) {
            seed = (seed * 0x5DEECE66DL + 0xB) & ((1L << 48) - 1);
            int x = 40 + (int) ((seed >>> 17) % 121);   // 40..160
            int y = MemoryEstimator.estimateMemory(stat, dataType, x);
            sum += x;
            sum2 += (long) x * x;
            err2 += (long) (x - y) * (x - y);
            checksum = checksum * 31 + y;
        }
        System.out.println("checksum=" + checksum);
        System.out.println("avg=" + (sum / size));
        System.out.println("err=" + Math.sqrt(1.0 * err2 / sum2));
        System.out.println("pct=" + MemoryEstimator.samplingPct(stat));
        System.out.println("calcPct=" + (dataType.getCount() * 100 / size));
        System.out.println("statsData=" + stat.get());
    }
}
