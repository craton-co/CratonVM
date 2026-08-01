import java.util.Random;

/**
 * Narrowing #2 for the JIT-only `ArrayIndexOutOfBoundsException: Index 0 out of
 * bounds for length 0`.
 *
 * Narrowing #1 (ArrayLenProbe) did NOT reproduce, and it differed from the
 * failing method in one structural way: the real loop is
 *
 *     for (int i = 0; i < size; i += pageSz)      // increment reads a variable
 *         pageSz = random.nextInt(48) + 1;        // ...assigned in the BODY
 *         Integer[] storage = createStorage(pageSz);
 *         ...
 *         use(storage, pageSz);                   // same var, third read site
 *
 * so `pageSz` is live across the back-edge and read at three sites. If the JIT
 * gets one of those reads wrong, `storage.length` and the `count` argument
 * disagree. This keeps that exact shape and no H2 dependency.
 *
 * Usage: ArrayLenProbe2 [size] [rounds]
 */
public class ArrayLenProbe2 {

    static Integer[] createStorage(int size) {
        return new Integer[size];
    }

    /** Mirrors MemoryEstimator's array overload: walks `count` elements. */
    static int consume(Integer[] storage, int count) {
        int index = 0;
        int memSum = 0;
        int cnt = count;
        while (cnt-- > 0) {
            Integer data = storage[index++];
            memSum += data == null ? 0 : data;
        }
        return memSum;
    }

    public static void main(String[] args) {
        int size = args.length > 0 ? Integer.parseInt(args[0]) : 10000;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        Random random = new Random();
        long mismatches = 0, iterations = 0, checksum = 0;
        int firstPageSz = -1, firstLen = -1;

        for (int r = 0; r < rounds; r++) {
            int pageSz;
            for (int i = 0; i < size; i += pageSz) {
                pageSz = random.nextInt(48) + 1;
                Integer[] storage = createStorage(pageSz);
                iterations++;
                if (storage.length != pageSz) {
                    mismatches++;
                    if (firstPageSz < 0) {
                        firstPageSz = pageSz;
                        firstLen = storage.length;
                        System.out.println("MISMATCH round=" + r + " i=" + i
                                + " pageSz=" + pageSz + " storage.length=" + storage.length);
                    }
                    continue;
                }
                int x = 0;
                for (int k = 0; k < pageSz; k++) {
                    storage[k] = 100 + k;
                    x += storage[k];
                }
                try {
                    checksum += consume(storage, pageSz) + x;
                } catch (RuntimeException e) {
                    mismatches++;
                    if (firstPageSz < 0) {
                        firstPageSz = pageSz;
                        firstLen = storage.length;
                        System.out.println("CONSUME THREW round=" + r + " i=" + i
                                + " pageSz=" + pageSz + " storage.length=" + storage.length
                                + " : " + e);
                    }
                }
            }
        }
        System.out.println("iterations=" + iterations + " mismatches=" + mismatches
                + " checksum=" + checksum);
        System.out.println(mismatches == 0 ? "PASS" : "FAIL");
    }
}
