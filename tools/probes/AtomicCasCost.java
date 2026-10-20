import java.lang.management.*;
import java.util.concurrent.atomic.AtomicLong;

/**
 * The premise the Random-shadow retirement was rejected on: "the JDK's own
 * Random keys on an AtomicLong whose get/compareAndSet are themselves natives
 * here". `next(int)` is a `get` + `compareAndSet` loop, so those two are the
 * cost that decides whether the shadow can be retired.
 */
public class AtomicCasCost {
    static final ThreadMXBean TB = ManagementFactory.getThreadMXBean();
    static final long Q = 15_625_000L; static final int MIN_TICKS = 40;
    static long sink;
    interface Arm { long run(int n); }
    static void time(String name, Arm arm) {
        arm.run(10_000);
        int n = 200_000;
        while (true) {
            long c0 = TB.getCurrentThreadCpuTime(); sink += arm.run(n);
            long d = TB.getCurrentThreadCpuTime() - c0;
            if (d >= Q*MIN_TICKS || n >= (1<<29)) break;
            long w = d==0?16:(Q*MIN_TICKS*2)/Math.max(d,1);
            n = (int) Math.min((long)n*Math.max(2,Math.min(w,64)), 1<<30);
        }
        long best = Long.MAX_VALUE;
        for (int r=0;r<3;r++){ long c0=TB.getCurrentThreadCpuTime(); sink+=arm.run(n);
            long d=TB.getCurrentThreadCpuTime()-c0; if(d<best)best=d; }
        System.out.printf("CK %-34s %9.2f ns/op cpu (n=%d ticks=%d)%n", name, (double)best/n, n, best/Q);
    }
    static final AtomicLong AL = new AtomicLong(1);
    /** Exactly java.util.Random.next(int)'s loop body, on a real AtomicLong. */
    static int lcgNext(AtomicLong seed, int bits) {
        long oldseed, nextseed;
        do {
            oldseed = seed.get();
            nextseed = (oldseed * 0x5DEECE66DL + 0xBL) & ((1L << 48) - 1);
        } while (!seed.compareAndSet(oldseed, nextseed));
        return (int) (nextseed >>> (48 - bits));
    }
    public static void main(String[] a) {
        System.out.println("CK AtomicCasCost");
        time("AL.get()",             n -> { long s=0; for(int i=0;i<n;i++) s+=AL.get(); return s; });
        time("AL.compareAndSet(v,v)",n -> { long s=0; for(int i=0;i<n;i++){ long v=AL.get(); if(AL.compareAndSet(v,v+1)) s++; } return s; });
        time("AL.getAndIncrement()", n -> { long s=0; for(int i=0;i<n;i++) s+=AL.getAndIncrement(); return s; });
        time("lcgNext(AL,32)  [= Random.next]", n -> { long s=0; for(int i=0;i<n;i++) s+=lcgNext(AL,32); return s; });
        // The same loop body INLINE, with no static-method boundary. If this is
        // fast and `lcgNext` above is not, the cost is the call, not the CAS.
        time("lcg loop inline (no call)", n -> {
            long s=0;
            for(int i=0;i<n;i++){
                long oldseed, nextseed;
                do { oldseed = AL.get(); nextseed = (oldseed * 0x5DEECE66DL + 0xBL) & ((1L<<48)-1); }
                while(!AL.compareAndSet(oldseed, nextseed));
                s += (int)(nextseed >>> 16);
            }
            return s; });
        // A CAS loop that can never fail, to price the do/while shape itself.
        time("CAS in a do/while (1 iter)", n -> { long s=0; for(int i=0;i<n;i++){ long v; do { v=AL.get(); } while(!AL.compareAndSet(v, v+1)); s++; } return s; });
        System.out.println("CK sink="+(sink==0?0:1));
    }
}
