import java.io.InputStream;
import java.lang.management.ManagementFactory;
import java.util.Random;
import java.util.concurrent.atomic.AtomicLong;

/** Exact results for the callee shapes behind JpaLargeBlobTest's timeout.
 * Run each arm in a fresh process; timing is separate from the checked result.
 */
public final class BlobCalleeAcceptance {
    static final long MASK = (1L << 48) - 1;
    static final long MULT = 0x5DEECE66DL;
    static final AtomicLong SEED = new AtomicLong(1);
    static final class Boxed extends InputStream {
        Long left;
        Boxed(long n) { left = n; }
        public int read() { if (left > 0) { left--; return 7; } return -1; }
    }
    static final class Primitive extends InputStream {
        long left;
        Primitive(long n) { left = n; }
        public int read() { if (left > 0) { left--; return 7; } return -1; }
    }
    static int next(AtomicLong seed) {
        long old, value;
        do {
            old = seed.get();
            value = (old * MULT + 11) & MASK;
        } while (!seed.compareAndSet(old, value));
        return (int)(value >>> 16);
    }
    static int nested(AtomicLong seed) { return next(seed); }
    static final class Virtual {
        int nextValue(AtomicLong seed) { return next(seed); }
    }
    static final Virtual VIRTUAL = new Virtual();
    public static void main(String[] args) throws Exception {
        String arm = System.getProperty("arm", "boxed");
        int n = Integer.getInteger("n", 1_000_000);
        long expected = 0, finalSeed = 1;
        if (arm.startsWith("atomic")) {
            for (int i = 0; i < n; i++) {
                finalSeed = (finalSeed * MULT + 11) & MASK;
                expected += (int)(finalSeed >>> 16);
            }
        } else if (arm.equals("boxed") || arm.equals("primitive")) {
            expected = 7L * n;
        } else if (arm.equals("constructors")) {
            expected = 3L * n * (n - 1) / 2;
        } else if (arm.equals("random")) {
            for (int i = 0; i < n; i++) {
                long seed = (i ^ MULT) & MASK;
                expected += (int)(((seed * MULT + 11) & MASK) >>> 16);
            }
        } else throw new IllegalArgumentException(arm);
        long cpu0 = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime();
        long wall0 = System.nanoTime();
        long sum = 0;
        switch (arm) {
            case "boxed":
            case "primitive": {
                InputStream in = arm.equals("boxed") ? new Boxed(n) : new Primitive(n);
                for (int i = 0; i < n; i++) sum += in.read();
                if (in.read() != -1 || in.read() != -1) throw new AssertionError("EOF");
                break;
            }
            case "atomic-static":
                for (int i = 0; i < n; i++) sum += next(SEED);
                break;
            case "atomic-nested":
                for (int i = 0; i < n; i++) sum += nested(SEED);
                break;
            case "atomic-virtual":
                for (int i = 0; i < n; i++) sum += VIRTUAL.nextValue(SEED);
                break;
            case "atomic-inline":
                for (int i = 0; i < n; i++) {
                    long old, value;
                    do {
                        old = SEED.get();
                        value = (old * MULT + 11) & MASK;
                    } while (!SEED.compareAndSet(old, value));
                    sum += (int)(value >>> 16);
                }
                break;
            case "constructors":
                for (int i = 0; i < n; i++) {
                    sum += new AtomicLong(i).get();
                    sum += new java.util.concurrent.atomic.AtomicInteger(i).get();
                    sum += new java.util.concurrent.atomic.AtomicReference<Integer>(i).get();
                    StringBuilder b = new StringBuilder();
                    if (b.length() != 0 || b.capacity() != 16)
                        throw new AssertionError("StringBuilder constructor");
                    if (!b.append(i).toString().equals(Integer.toString(i)))
                        throw new AssertionError("StringBuilder contents");
                }
                break;
            case "random":
                for (int i = 0; i < n; i++) sum += new Random(i).nextInt();
                break;
            default: throw new IllegalArgumentException(arm);
        }
        long wall = System.nanoTime() - wall0;
        long cpu = ManagementFactory.getThreadMXBean().getCurrentThreadCpuTime() - cpu0;
        if (sum != expected) throw new AssertionError("sum " + sum + " expected " + expected);
        if (arm.startsWith("atomic") && SEED.get() != finalSeed)
            throw new AssertionError("seed " + SEED.get() + " expected " + finalSeed);
        System.out.println("RESULT arm=" + arm + " n=" + n + " sum=" + sum + " PASS");
        System.out.println("TIMING cpu_ns=" + cpu + " wall_ns=" + wall);
    }
}
