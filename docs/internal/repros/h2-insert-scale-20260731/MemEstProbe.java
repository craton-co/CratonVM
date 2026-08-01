import java.util.concurrent.atomic.AtomicLong;

/**
 * Deterministic standalone witness for org.h2.util.MemoryEstimator's arithmetic.
 *
 * Verbatim copy of H2's estimator math (bit-packed long stats, variable long
 * shifts, long division, AtomicLong CAS) driven by a FIXED-seed pseudo-random
 * sequence, so the whole run is a pure function with an exact checksum.
 *
 * H2's own TestMemoryEstimator is statistical (unseeded Random) and asserts
 * `samplingPct <= 7`; this probe reports the same quantities plus an exact
 * checksum so a divergence is a diff, not a distribution.
 *
 * Usage: MemEstProbe [iterations]
 */
public class MemEstProbe {

    private static final int SKIP_SUM_SHIFT = 8;
    private static final int COUNTER_MASK = (1 << SKIP_SUM_SHIFT) - 1;
    private static final int SKIP_SUM_MASK = 0xFFFF;
    private static final int INIT_BIT_SHIFT = 24;
    private static final int INIT_BIT = 1 << INIT_BIT_SHIFT;
    private static final int WINDOW_SHIFT = 8;
    private static final int MAGNITUDE_LIMIT = WINDOW_SHIFT - 1;
    private static final int WINDOW_SIZE = 1 << WINDOW_SHIFT;
    private static final int WINDOW_HALF_SIZE = WINDOW_SIZE >> 1;
    private static final int SUM_SHIFT = 32;

    /** Number of times the "real" measurement ran instead of being estimated. */
    private static int calcCount;

    static int estimateMemory(AtomicLong stats, int data) {
        long statsData = stats.get();
        int counter = getCounter(statsData);
        int skipSum = getSkipSum(statsData);
        long initialized = statsData & INIT_BIT;
        long sum = statsData >>> SUM_SHIFT;
        int mem = 0;
        int cnt = 0;
        if (initialized == 0 || counter-- == 0) {
            cnt = 1;
            mem = data;
            ++calcCount;
            long delta = ((long) mem << WINDOW_SHIFT) - sum;
            if (initialized == 0) {
                if (++counter == WINDOW_SIZE) {
                    initialized = INIT_BIT;
                }
                sum = (sum * counter + delta + (counter >> 1)) / counter;
            } else {
                long absDelta = delta >= 0 ? delta : -delta;
                int magnitude = calculateMagnitude(sum, absDelta);
                sum += ((delta >> (MAGNITUDE_LIMIT - magnitude)) + 1) >> 1;
                counter = ((1 << magnitude) - 1) & COUNTER_MASK;

                delta = (counter << WINDOW_SHIFT) - skipSum;
                skipSum += (delta + WINDOW_HALF_SIZE) >> WINDOW_SHIFT;
            }
        }
        long updated = updateStatsData(stats, statsData, counter, skipSum, initialized, sum, cnt, mem);
        return getAverage(updated);
    }

    static int samplingPct(AtomicLong stats) {
        long statsData = stats.get();
        int count = (statsData & INIT_BIT) == 0 ? getCounter(statsData) : WINDOW_SIZE;
        int total = getSkipSum(statsData) + count;
        return (count * 100 + (total >> 1)) / total;
    }

    private static int calculateMagnitude(long sum, long absDelta) {
        int magnitude = 0;
        while (absDelta < sum && magnitude < MAGNITUDE_LIMIT) {
            ++magnitude;
            absDelta <<= 1;
        }
        return magnitude;
    }

    private static long updateStatsData(AtomicLong stats, long statsData,
                                        int counter, int skipSum, long initialized, long sum,
                                        int itemsCount, int itemsMem) {
        return updateStatsData(stats, statsData,
                constructStatsData(sum, initialized, skipSum, counter), itemsCount, itemsMem);
    }

    private static long constructStatsData(long sum, long initialized, int skipSum, int counter) {
        return (sum << SUM_SHIFT) | initialized | ((long) skipSum << SKIP_SUM_SHIFT) | counter;
    }

    private static long updateStatsData(AtomicLong stats, long statsData, long updatedStatsData,
                                        int itemsCount, int itemsMem) {
        while (!stats.compareAndSet(statsData, updatedStatsData)) {
            statsData = stats.get();
            long sum = statsData >>> SUM_SHIFT;
            if (itemsCount > 0) {
                sum += itemsMem - ((sum * itemsCount + WINDOW_HALF_SIZE) >> WINDOW_SHIFT);
            }
            updatedStatsData = (sum << SUM_SHIFT) | (statsData & (INIT_BIT | SKIP_SUM_MASK | COUNTER_MASK));
        }
        return updatedStatsData;
    }

    private static int getCounter(long statsData) {
        return (int) (statsData & COUNTER_MASK);
    }

    private static int getSkipSum(long statsData) {
        return (int) ((statsData >> SKIP_SUM_SHIFT) & SKIP_SUM_MASK);
    }

    private static int getAverage(long updatedStatsData) {
        return (int) (updatedStatsData >>> (SUM_SHIFT + WINDOW_SHIFT));
    }

    public static void main(String[] args) {
        int size = args.length > 0 ? Integer.parseInt(args[0]) : 10000;
        // Fixed-seed LCG standing in for the test's gaussian sequence: same
        // shape (mean ~100, spread ~30), but reproducible on every VM.
        long seed = 0x5DEECE66DL;
        AtomicLong stat = new AtomicLong();
        long sum = 0;
        long sum2 = 0;
        long err2 = 0;
        long checksum = 0;
        for (int i = 0; i < size; i++) {
            seed = (seed * 0x5DEECE66DL + 0xB) & ((1L << 48) - 1);
            int x = 40 + (int) ((seed >>> 17) % 121);   // 40..160
            int y = estimateMemory(stat, x);
            sum += x;
            sum2 += (long) x * x;
            err2 += (long) (x - y) * (x - y);
            checksum = checksum * 31 + y;
        }
        long avg = sum / size;
        double err = Math.sqrt(1.0 * err2 / sum2);
        int pct = samplingPct(stat);
        System.out.println("checksum=" + checksum);
        System.out.println("avg=" + avg);
        System.out.println("err=" + err);
        System.out.println("pct=" + pct);
        System.out.println("calcPct=" + (calcCount * 100 / size));
        System.out.println("statsData=" + stat.get());
    }
}
