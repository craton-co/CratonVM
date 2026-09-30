import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.atomic.*;

/** Is `new AtomicLong(i)` 1.7us because of AtomicLong, or because of allocation shape? */
public class AtomicCtorSplit {
    static final ThreadMXBean TB = ManagementFactory.getThreadMXBean();
    static final long Q = 15_625_000L;
    static final int MIN_TICKS = Integer.getInteger("minticks", 40);
    static long sink; static Object osink;
    interface Arm { long run(int n); }
    static void time(String name, Arm arm) {
        arm.run(10_000);
        int n = Integer.getInteger("iters", 200_000);
        while (true) {
            long c0 = TB.getCurrentThreadCpuTime(); sink += arm.run(n);
            long d = TB.getCurrentThreadCpuTime() - c0;
            if (d >= Q * MIN_TICKS || n >= (1 << 29)) break;
            long want = d == 0 ? 16 : (Q * MIN_TICKS * 2) / Math.max(d, 1);
            n = (int) Math.min((long) n * Math.max(2, Math.min(want, 64)), 1 << 30);
        }
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 3; r++) {
            long c0 = TB.getCurrentThreadCpuTime(); sink += arm.run(n);
            long d = TB.getCurrentThreadCpuTime() - c0; if (d < best) best = d;
        }
        System.out.printf("CK %-32s %9.2f ns/op cpu (n=%d ticks=%d)%n", name, (double) best/n, n, best/Q);
    }
    /** A hand-rolled stand-in with the SAME shape as AtomicLong: one volatile long field. */
    static final class PlainVolatileLong { volatile long v; PlainVolatileLong(long x){ v = x; } }
    static final class PlainLong { long v; PlainLong(long x){ v = x; } }
    static final AtomicLong AL = new AtomicLong(1);
    public static void main(String[] a) {
        System.out.println("CK AtomicCtorSplit");
        time("new Object()",            n -> { long s=0; for(int i=0;i<n;i++){ osink=new Object(); s++; } return s; });
        time("new PlainLong(i)",        n -> { long s=0; for(int i=0;i<n;i++){ osink=new PlainLong(i); s++; } return s; });
        time("new PlainVolatileLong(i)",n -> { long s=0; for(int i=0;i<n;i++){ osink=new PlainVolatileLong(i); s++; } return s; });
        time("new AtomicInteger(i)",    n -> { long s=0; for(int i=0;i<n;i++){ osink=new AtomicInteger(i); s++; } return s; });
        time("new AtomicLong(i)",       n -> { long s=0; for(int i=0;i<n;i++){ osink=new AtomicLong(i); s++; } return s; });
        time("new AtomicReference(nul)",n -> { long s=0; for(int i=0;i<n;i++){ osink=new AtomicReference<>(); s++; } return s; });
        time("AL.get()",                n -> { long s=0; for(int i=0;i<n;i++) s += AL.get(); return s; });
        time("AL.incrementAndGet()",    n -> { long s=0; for(int i=0;i<n;i++) s += AL.incrementAndGet(); return s; });
        System.out.println("CK sink="+(sink==0?0:1)+" osink="+(osink==null?0:1));
    }
}
