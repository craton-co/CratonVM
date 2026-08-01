import java.nio.ByteBuffer;
import java.util.Random;

import org.h2.mvstore.WriteBuffer;
import org.h2.mvstore.type.BasicDataType;

/**
 * Narrows the JIT-only `ArrayIndexOutOfBoundsException: Index 0 out of bounds
 * for length 0` that ExactEstimatorProbe hits in testPageEstimator.
 *
 * The failing shape is
 *     int pageSz = random.nextInt(48) + 1;      // >= 1
 *     Integer[] storage = dataType.createStorage(pageSz);   // generic T[] via a bridge
 *     for (int k = 0; k < pageSz; k++) { storage[k] = ...; }
 *     ... estimateMemory(..., storage, pageSz)  // indexes storage[0]
 *
 * so `storage.length` disagreed with `pageSz`. This checks that invariant
 * DIRECTLY on every iteration and reports the first disagreement with both
 * values, which distinguishes:
 *   * createStorage returning a wrong-length array (length != pageSz), from
 *   * pageSz itself being read inconsistently at different sites, from
 *   * the inner fill loop being skipped.
 *
 * Usage: ArrayLenProbe [rounds]
 */
public class ArrayLenProbe {

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
        Random random = new Random();
        TestDataType dataType = new TestDataType();

        long badLen = 0, badFill = 0, zeroPage = 0, iters = 0;
        int firstBadPageSz = -1, firstBadLen = -1, firstBadIter = -1;

        for (int i = 0; i < rounds; i++) {
            int pageSz = random.nextInt(48) + 1;
            if (pageSz < 1) {
                zeroPage++;
                continue;
            }
            Integer[] storage = dataType.createStorage(pageSz);
            iters++;
            int len = storage.length;
            if (len != pageSz) {
                badLen++;
                if (firstBadIter < 0) {
                    firstBadIter = i;
                    firstBadPageSz = pageSz;
                    firstBadLen = len;
                    System.out.println("LENGTH MISMATCH at i=" + i
                            + " pageSz=" + pageSz + " storage.length=" + len);
                }
                continue;
            }
            int filled = 0;
            for (int k = 0; k < pageSz; k++) {
                storage[k] = 100 + k;
                filled++;
            }
            if (filled != pageSz) {
                badFill++;
                if (badFill == 1) {
                    System.out.println("FILL COUNT MISMATCH at i=" + i
                            + " pageSz=" + pageSz + " filled=" + filled);
                }
            }
        }

        System.out.println("rounds=" + rounds + " iters=" + iters);
        System.out.println("zeroPageSz=" + zeroPage);
        System.out.println("lengthMismatch=" + badLen
                + (firstBadIter >= 0 ? " (first i=" + firstBadIter + " pageSz=" + firstBadPageSz
                        + " len=" + firstBadLen + ")" : ""));
        System.out.println("fillMismatch=" + badFill);
        System.out.println(badLen == 0 && badFill == 0 && zeroPage == 0 ? "PASS" : "FAIL");
    }
}
