import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * In the two-level (non-inlined) shape, does arg1 arrive correctly?
 * Distinguishes "the compare is done at the wrong width/signedness" from
 * "the second argument is garbage".
 *
 * usage: Arg1Probe <seconds>
 */
public class Arg1Probe {

    static final int NEG = -536870912;
    static final AtomicInteger src = new AtomicInteger(NEG);

    private static int a0(int a, int b)   { return a; }
    private static int a1(int a, int b)   { return b; }
    private static int sum(int a, int b)  { return a + b; }
    private static int a1of3(int a, int b, int c) { return b; }
    private static int a2of3(int a, int b, int c) { return c; }
    private static boolean ltZero(int a, int b)  { return a < b; }
    private static boolean gtZero(int a, int b)  { return a > b; }
    private static boolean eqZero(int a, int b)  { return a == b; }
    private static boolean geNeg1(int a, int b)  { return a >= b; }

    private static int f_a0()  { return a0(src.get(), 0); }
    private static int f_a1()  { return a1(src.get(), 0); }
    private static int f_a1b() { return a1(src.get(), 12345); }
    private static int f_sum() { return sum(src.get(), 0); }
    private static int f_a1of3() { return a1of3(src.get(), 111, 222); }
    private static int f_a2of3() { return a2of3(src.get(), 111, 222); }
    private static boolean f_lt()  { return ltZero(src.get(), 0); }   // want true  (neg < 0)
    private static boolean f_gt()  { return gtZero(src.get(), 0); }   // want false
    private static boolean f_eq()  { return eqZero(src.get(), 0); }   // want false
    private static boolean f_geNeg1() { return geNeg1(src.get(), -1); } // want false (neg >= -1)

    static final AtomicLong[] bad = new AtomicLong[12];
    static final int[] saw = new int[12];
    static final AtomicLong loops = new AtomicLong();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int secs = args.length > 0 ? Integer.parseInt(args[0]) : 6;
        for (int i = 0; i < bad.length; i++) {
            bad[i] = new AtomicLong();
        }
        for (int i = 0; i < 4; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 200; k++) { src.incrementAndGet(); }
                    for (int k = 0; k < 200; k++) { src.decrementAndGet(); }
                }
            });
            t.setDaemon(true);
            t.start();
        }
        for (int i = 0; i < 4; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 5000; k++) {
                        int v;
                        v = f_a0();    if (v >= 0)      { bad[0].incrementAndGet(); saw[0] = v; }
                        v = f_a1();    if (v != 0)      { bad[1].incrementAndGet(); saw[1] = v; }
                        v = f_a1b();   if (v != 12345)  { bad[2].incrementAndGet(); saw[2] = v; }
                        v = f_sum();   if (v >= 0)      { bad[3].incrementAndGet(); saw[3] = v; }
                        v = f_a1of3(); if (v != 111)    { bad[4].incrementAndGet(); saw[4] = v; }
                        v = f_a2of3(); if (v != 222)    { bad[5].incrementAndGet(); saw[5] = v; }
                        if (!f_lt())     { bad[6].incrementAndGet(); }
                        if (f_gt())      { bad[7].incrementAndGet(); }
                        if (f_eq())      { bad[8].incrementAndGet(); }
                        if (f_geNeg1())  { bad[9].incrementAndGet(); }
                    }
                    loops.addAndGet(5000);
                }
            });
            t.setDaemon(true);
            t.start();
        }
        Thread.sleep(secs * 1000L);
        stop = true;
        Thread.sleep(300);
        String[] d = { "a0(src,0) -> arg0", "a1(src,0) -> arg1 want 0", "a1(src,12345) want 12345",
                "sum(src,0) want negative", "a1of3(src,111,222) want 111", "a2of3(src,111,222) want 222",
                "ltZero(src,0) want TRUE", "gtZero(src,0) want false", "eqZero(src,0) want false",
                "geNeg1(src,-1) want false" };
        System.out.println("loops=" + loops.get() + " srcNow=" + src.get());
        for (int i = 0; i < d.length; i++) {
            System.out.println(String.format("  %-32s bad=%-10d %s", d[i], bad[i].get(),
                    (i <= 5 && bad[i].get() > 0) ? "lastSaw=" + saw[i] + " (0x" + Integer.toHexString(saw[i]) + ")" : ""));
        }
        long tot = 0;
        for (int i = 0; i < d.length; i++) { tot += bad[i].get(); }
        System.exit(tot > 0 ? 3 : 0);
    }
}
