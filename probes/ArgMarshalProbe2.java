import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * v2 of the argument-marshalling probe: same shapes, but now the source is
 * CONCURRENTLY MUTATED (which v1 lacked and which turns out to be required),
 * and the callee RETURNS what it received so the corrupted value is visible.
 *
 * Every source stays in [NEG, NEG+200] -- always negative, so any non-negative
 * value arriving in the callee is a wrong answer, and `saw` shows exactly what.
 *
 * usage: ArgMarshalProbe2 <seconds> [mutators]
 */
public class ArgMarshalProbe2 {

    static final int NEG = -536870912;
    static final int LO = NEG;
    static final int HI = NEG + 200;

    static final AtomicInteger atomicSrc = new AtomicInteger(NEG);
    static int plainStatic = NEG;
    static volatile int volatileStatic = NEG;

    private static int viaAtomic() {
        return atomicSrc.get();
    }

    private static int viaPlain() {
        return plainStatic;
    }

    private static int recv2(int a, int b) {
        return a;
    }

    private static boolean ge(int c, int s) {
        return c >= s;
    }

    // arg0 produced by a Java call
    private static int f1() { return recv2(viaAtomic(), 0); }
    // arg0 produced by a getstatic
    private static int f2() { return recv2(plainStatic, 0); }
    // arg0 produced by a volatile getstatic
    private static int f3() { return recv2(volatileStatic, 0); }
    // arg0 from a local  [control]
    private static int f4() { int c = plainStatic; return recv2(c, 0); }
    // arg0 produced by an AtomicInteger.get() directly
    private static int f5() { return recv2(atomicSrc.get(), 0); }
    // the original predicate, for reference
    private static boolean f6() { return ge(atomicSrc.get(), 0); }

    static final AtomicLong[] bad = new AtomicLong[8];
    static final int[] saw = new int[8];
    static final AtomicLong loops = new AtomicLong();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int secs = args.length > 0 ? Integer.parseInt(args[0]) : 8;
        int nMut = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        for (int i = 0; i < bad.length; i++) {
            bad[i] = new AtomicLong();
        }

        for (int i = 0; i < nMut; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 200; k++) {
                        atomicSrc.incrementAndGet();
                        plainStatic++;
                        volatileStatic++;
                    }
                    for (int k = 0; k < 200; k++) {
                        atomicSrc.decrementAndGet();
                        plainStatic--;
                        volatileStatic--;
                    }
                }
            }, "mut-" + i);
            t.setDaemon(true);
            t.start();
        }

        for (int i = 0; i < 4; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 5000; k++) {
                        int v;
                        v = f1(); if (v >= 0) { bad[1].incrementAndGet(); saw[1] = v; }
                        v = f2(); if (v >= 0) { bad[2].incrementAndGet(); saw[2] = v; }
                        v = f3(); if (v >= 0) { bad[3].incrementAndGet(); saw[3] = v; }
                        v = f4(); if (v >= 0) { bad[4].incrementAndGet(); saw[4] = v; }
                        v = f5(); if (v >= 0) { bad[5].incrementAndGet(); saw[5] = v; }
                        if (f6())  { bad[6].incrementAndGet(); }
                    }
                    loops.addAndGet(5000);
                }
            }, "read-" + i);
            t.setDaemon(true);
            t.start();
        }

        Thread.sleep(secs * 1000L);
        stop = true;
        Thread.sleep(300);

        String[] d = { "", "f1 recv2(viaAtomic(),0)  arg0 <- java call",
                "f2 recv2(plainStatic,0)  arg0 <- getstatic",
                "f3 recv2(volatileStatic,0)  arg0 <- volatile getstatic",
                "f4 recv2(local,0)  [control]",
                "f5 recv2(atomicSrc.get(),0)  arg0 <- invokevirtual",
                "f6 ge(atomicSrc.get(),0)  want false" };
        System.out.println("loops=" + loops.get() + "  valid range [" + LO + "," + HI + "]");
        for (int i = 1; i <= 6; i++) {
            System.out.println("  " + d[i] + "  bad=" + bad[i].get()
                    + (i < 6 && bad[i].get() > 0 ? "  lastSaw=" + saw[i] + " (0x" + Integer.toHexString(saw[i]) + ")" : ""));
        }
        System.out.println("  atomicNow=" + atomicSrc.get() + " plainNow=" + plainStatic
                + " volatileNow=" + volatileStatic);
        long tot = 0;
        for (int i = 1; i <= 6; i++) {
            tot += bad[i].get();
        }
        System.exit(tot > 0 ? 3 : 0);
    }
}
