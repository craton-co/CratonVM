import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * What value does the miscompiled callee actually return?
 *
 * geInt has the SAME bytecode shape as ge (if_icmplt / iconst_1 / goto / iconst_0)
 * but an int return, so the raw returned bits are printable.
 *
 * usage: ReturnValueProbe <seconds>
 */
public class ReturnValueProbe {

    static final int NEG = -536870912;
    static final AtomicInteger src = new AtomicInteger(NEG);

    private static boolean ge(int c, int s)  { return c >= s; }
    private static int geInt(int c, int s)   { return c >= s ? 1 : 0; }
    private static int constOne(int c, int s) { return 1; }
    private static int constZero(int c, int s) { return 0; }

    private static boolean f_ge()      { return ge(src.get(), 0); }
    private static int     f_geInt()   { return geInt(src.get(), 0); }
    private static int     f_one()     { return constOne(src.get(), 0); }
    private static int     f_zero()    { return constZero(src.get(), 0); }

    static final AtomicLong badGe = new AtomicLong();
    static final AtomicLong badGeInt = new AtomicLong();
    static final AtomicLong badOne = new AtomicLong();
    static final AtomicLong badZero = new AtomicLong();
    static volatile int sawGeInt = 12345;
    static volatile int sawOne = 12345;
    static volatile int sawZero = 12345;
    static final AtomicLong loops = new AtomicLong();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int secs = args.length > 0 ? Integer.parseInt(args[0]) : 6;

        for (int i = 0; i < 4; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 200; k++) {
                        src.incrementAndGet();
                    }
                    for (int k = 0; k < 200; k++) {
                        src.decrementAndGet();
                    }
                }
            });
            t.setDaemon(true);
            t.start();
        }

        for (int i = 0; i < 4; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 5000; k++) {
                        if (f_ge()) {
                            badGe.incrementAndGet();
                        }
                        int v = f_geInt();
                        if (v != 0) {
                            badGeInt.incrementAndGet();
                            sawGeInt = v;
                        }
                        int o = f_one();
                        if (o != 1) {
                            badOne.incrementAndGet();
                            sawOne = o;
                        }
                        int z = f_zero();
                        if (z != 0) {
                            badZero.incrementAndGet();
                            sawZero = z;
                        }
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
        System.out.println("loops=" + loops.get() + " srcNow=" + src.get());
        System.out.println("  f_ge()    want false  bad=" + badGe.get());
        System.out.println("  f_geInt() want 0      bad=" + badGeInt.get()
                + "  lastSaw=" + sawGeInt + " (0x" + Integer.toHexString(sawGeInt) + ")");
        System.out.println("  f_one()   want 1      bad=" + badOne.get()
                + "  lastSaw=" + sawOne + " (0x" + Integer.toHexString(sawOne) + ")");
        System.out.println("  f_zero()  want 0      bad=" + badZero.get()
                + "  lastSaw=" + sawZero + " (0x" + Integer.toHexString(sawZero) + ")");
        System.exit(badGe.get() + badGeInt.get() + badOne.get() + badZero.get() > 0 ? 3 : 0);
    }
}
