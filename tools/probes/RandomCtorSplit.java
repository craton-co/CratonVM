import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.Random;
import java.util.concurrent.atomic.AtomicLong;

/** Where does `new Random(seed)` spend 1504 ns? Split the constructor itself. */
public class RandomCtorSplit {
    static final ThreadMXBean TB = ManagementFactory.getThreadMXBean();
    static final long Q = 15_625_000L;
    static final int MIN_TICKS = Integer.getInteger("minticks", 40);
    static long sink;
    interface Arm { long run(int n); }

    static void time(String name, Arm arm) {
        arm.run(10_000);
        int n = Integer.getInteger("iters", 200_000);
        while (true) {
            long c0 = TB.getCurrentThreadCpuTime();
            sink += arm.run(n);
            long d = TB.getCurrentThreadCpuTime() - c0;
            if (d >= Q * MIN_TICKS || n >= (1 << 29)) break;
            long want = d == 0 ? 16 : (Q * MIN_TICKS * 2) / Math.max(d, 1);
            n = (int) Math.min((long) n * Math.max(2, Math.min(want, 64)), 1 << 30);
        }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 3; r++) {
            long c0 = TB.getCurrentThreadCpuTime();
            sink += arm.run(n);
            long d = TB.getCurrentThreadCpuTime() - c0;
            if (d < best) best = d;
        }
        System.out.printf("CK %-32s %9.2f ns/op cpu (n=%d ticks=%d)%n",
            name, (double) best / n, n, best / Q);
    }

    static final Random SHARED = new Random(7);
    static Object osink;

    public static void main(String[] a) {
        System.out.println("CK RandomCtorSplit");
        time("new Object()",            n -> { long s=0; for (int i=0;i<n;i++){ osink=new Object(); s++; } return s; });
        time("new Object().hashCode()", n -> { long s=0; for (int i=0;i<n;i++) s += new Object().hashCode(); return s; });
        time("new AtomicLong(i)",       n -> { long s=0; for (int i=0;i<n;i++){ osink=new AtomicLong(i); s++; } return s; });
        time("SHARED.hashCode()",       n -> { long s=0; for (int i=0;i<n;i++) s += SHARED.hashCode(); return s; });
        time("new Random(i)  [ctor only]", n -> { long s=0; for (int i=0;i<n;i++){ osink=new Random(i); s++; } return s; });
        time("new Random(i).nextInt()", n -> { long s=0; for (int i=0;i<n;i++) s += new Random(i).nextInt(); return s; });
        time("SHARED.nextInt()",        n -> { long s=0; for (int i=0;i<n;i++) s += SHARED.nextInt(); return s; });
        time("SHARED.setSeed(i)",       n -> { long s=0; for (int i=0;i<n;i++){ SHARED.setSeed(i); s++; } return s; });
        System.out.println("CK sink=" + (sink==0?0:1) + " osink=" + (osink==null?0:1));
    }
}
