import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.util.concurrent.atomic.*;

/**
 * Is the ~1650 ns construction cost about `java.util.concurrent.atomic`, about
 * the superclass, or about JDK classes in general?
 */
public class CtorClassSplit {
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
        System.out.printf("CK %-34s %9.2f ns/op cpu (n=%d ticks=%d)%n", name, (double) best/n, n, best/Q);
    }
    // app-classpath shapes
    static final class AppPlain { volatile long v; AppPlain(long x){ v = x; } }
    static class AppNumber extends Number {
        volatile long v; AppNumber(long x){ v = x; }
        public int intValue(){ return (int) v; } public long longValue(){ return v; }
        public float floatValue(){ return v; } public double doubleValue(){ return v; }
    }
    /** App-classpath clone of AtomicLong's exact shape: extends Number, implements Serializable. */
    static final class AppAtomicLike extends Number implements java.io.Serializable {
        private static final long serialVersionUID = 1L;
        private volatile long value;
        AppAtomicLike(long initialValue){ value = initialValue; }
        public int intValue(){ return (int) value; } public long longValue(){ return value; }
        public float floatValue(){ return value; } public double doubleValue(){ return value; }
    }
    public static void main(String[] a) {
        System.out.println("CK CtorClassSplit");
        time("app  new AppPlain(i)",        n -> { long s=0; for(int i=0;i<n;i++){ osink=new AppPlain(i); s++; } return s; });
        time("app  new AppNumber(i)",       n -> { long s=0; for(int i=0;i<n;i++){ osink=new AppNumber(i); s++; } return s; });
        time("app  new AppAtomicLike(i)",   n -> { long s=0; for(int i=0;i<n;i++){ osink=new AppAtomicLike(i); s++; } return s; });
        time("jdk  new AtomicLong(i)",      n -> { long s=0; for(int i=0;i<n;i++){ osink=new AtomicLong(i); s++; } return s; });
        time("jdk  new java.util.Date(i)",  n -> { long s=0; for(int i=0;i<n;i++){ osink=new java.util.Date(i); s++; } return s; });
        time("jdk  new StringBuilder()",    n -> { long s=0; for(int i=0;i<n;i++){ osink=new StringBuilder(); s++; } return s; });
        time("jdk  new Long(i) [valueOf]",  n -> { long s=0; for(int i=0;i<n;i++){ osink=Long.valueOf(i + 1000); s++; } return s; });
        time("jdk  new java.awt.Point()",   n -> { long s=0; for(int i=0;i<n;i++){ osink=new java.awt.Point(i,i); s++; } return s; });
        System.out.println("CK sink="+(sink==0?0:1)+" osink="+(osink==null?0:1));
    }
}
