import java.lang.management.*;

/**
 * What does a Java-to-Java call cost when it is not inlined?
 *
 * The blob page attributes its cost to a "per-native-call floor". This asks the
 * same question of ORDINARY Java calls, because `lcgNext` — a static method
 * whose body is one `AtomicLong.get` plus one `compareAndSet`, both of which
 * measure 2.6 and 15.4 ns inline — costs 2265 ns when called.
 */
public class CallFloor {
    static final ThreadMXBean TB = ManagementFactory.getThreadMXBean();
    static final long Q = 15_625_000L; static final int MIN_TICKS = 40;
    static long sink;
    interface Arm { long run(int n); }
    static void time(String name, Arm arm) {
        arm.run(10_000);
        int n = 200_000;
        while (true) {
            long c0=TB.getCurrentThreadCpuTime(); sink+=arm.run(n);
            long d=TB.getCurrentThreadCpuTime()-c0;
            if (d>=Q*MIN_TICKS || n>=(1<<29)) break;
            long w = d==0?16:(Q*MIN_TICKS*2)/Math.max(d,1);
            n=(int)Math.min((long)n*Math.max(2,Math.min(w,64)), 1<<30);
        }
        long best=Long.MAX_VALUE;
        for (int r=0;r<3;r++){ long c0=TB.getCurrentThreadCpuTime(); sink+=arm.run(n);
            long d=TB.getCurrentThreadCpuTime()-c0; if(d<best)best=d; }
        System.out.printf("CK %-36s %9.2f ns/op cpu (n=%d ticks=%d)%n", name, (double)best/n, n, best/Q);
    }
    static long addStatic(long a, long b) { return a + b; }
    static long addStaticLoop(long a, long b) { long s = 0; for (int i = 0; i < 1; i++) s = a + b; return s; }
    static class Base { long v(long a){ return a + 1; } }
    static final Base B = new Base();
    public static void main(String[] a) {
        System.out.println("CK CallFloor");
        time("inline add (no call)",        n -> { long s=0; for(int i=0;i<n;i++) s+=i+1; return s; });
        time("static call, straight-line",  n -> { long s=0; for(int i=0;i<n;i++) s+=addStatic(i,1); return s; });
        time("static call, body has a LOOP",n -> { long s=0; for(int i=0;i<n;i++) s+=addStaticLoop(i,1); return s; });
        time("virtual call",                n -> { long s=0; for(int i=0;i<n;i++) s+=B.v(i); return s; });
        System.out.println("CK sink="+(sink==0?0:1));
    }
}
