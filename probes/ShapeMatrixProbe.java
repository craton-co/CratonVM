import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

/**
 * Shape matrix for the wrong-boolean miscompilation found via Tomcat's
 * ThreadPoolExecutor.isShutdown(). ctl only ever holds RUNNING..RUNNING+200,
 * all negative, so every shape below must always answer false.
 *
 * usage: ShapeMatrixProbe <seconds>
 */
public class ShapeMatrixProbe {

    static final int RUNNING = -1 << (Integer.SIZE - 3);
    static final int SHUTDOWN = 0;

    static final AtomicInteger ctl = new AtomicInteger(RUNNING);
    static int plainStatic = RUNNING;
    int instanceField = RUNNING;

    private static boolean ge(int c, int s) {
        return c >= s;
    }

    private static int identity(int c) {
        return c;
    }

    // S1 — Tomcat's exact shape: nested static call, arg from a getstatic + virtual call
    private static boolean s1() {
        return ge(ctl.get(), SHUTDOWN);
    }

    // S2 — same, but the arg goes through a local first
    private static boolean s2() {
        int c = ctl.get();
        return ge(c, SHUTDOWN);
    }

    // S3 — no nested call at all
    private static boolean s3() {
        return ctl.get() >= SHUTDOWN;
    }

    // S4 — nested static call, arg from a plain static int (no AtomicInteger)
    private static boolean s4() {
        return ge(plainStatic, SHUTDOWN);
    }

    // S5 — nested static call, arg from an int-returning static call
    private static boolean s5() {
        return ge(identity(plainStatic), SHUTDOWN);
    }

    // S6 — nested static call, arg passed in by the caller
    private static boolean s6(int c) {
        return ge(c, SHUTDOWN);
    }

    // S7 — nested static call, result stored to a local before return
    private static boolean s7() {
        boolean b = ge(ctl.get(), SHUTDOWN);
        return b;
    }

    // S8 — nested static call, second operand a non-constant
    private static boolean s8(int s) {
        return ge(ctl.get(), s);
    }

    static final AtomicLong[] wrong = new AtomicLong[9];
    static final AtomicLong calls = new AtomicLong();
    static volatile boolean stop = false;

    public static void main(String[] args) throws Exception {
        int secs = args.length > 0 ? Integer.parseInt(args[0]) : 10;
        for (int i = 0; i < wrong.length; i++) {
            wrong[i] = new AtomicLong();
        }
        ShapeMatrixProbe inst = new ShapeMatrixProbe();

        for (int i = 0; i < 4; i++) {
            Thread t = new Thread(() -> {
                while (!stop) {
                    for (int k = 0; k < 200; k++) {
                        ctl.incrementAndGet();
                        plainStatic++;
                    }
                    for (int k = 0; k < 200; k++) {
                        ctl.decrementAndGet();
                        plainStatic--;
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
                        if (s1()) {
                            wrong[1].incrementAndGet();
                        }
                        if (s2()) {
                            wrong[2].incrementAndGet();
                        }
                        if (s3()) {
                            wrong[3].incrementAndGet();
                        }
                        if (s4()) {
                            wrong[4].incrementAndGet();
                        }
                        if (s5()) {
                            wrong[5].incrementAndGet();
                        }
                        if (s6(ctl.get())) {
                            wrong[6].incrementAndGet();
                        }
                        if (s7()) {
                            wrong[7].incrementAndGet();
                        }
                        if (s8(SHUTDOWN)) {
                            wrong[8].incrementAndGet();
                        }
                    }
                    calls.addAndGet(5000);
                }
            }, "read-" + i);
            t.setDaemon(true);
            t.start();
        }

        Thread.sleep(secs * 1000L);
        stop = true;
        Thread.sleep(300);
        StringBuilder sb = new StringBuilder("callsPerShape=" + calls.get());
        String[] desc = { "", "s1 ge(ctl.get(),0)", "s2 local then ge", "s3 ctl.get()>=0 inline",
                "s4 ge(plainStatic,0)", "s5 ge(identity(x),0)", "s6 ge(param,0)",
                "s7 ge(...) via local bool", "s8 ge(ctl.get(),nonConstS)" };
        for (int i = 1; i < wrong.length; i++) {
            sb.append("\n  ").append(desc[i]).append(" wrong=").append(wrong[i].get());
        }
        sb.append("\n  ctlNow=").append(ctl.get()).append(" plainStatic=").append(plainStatic)
                .append(" instField=").append(inst.instanceField);
        System.out.println(sb);
        long tot = 0;
        for (int i = 1; i < wrong.length; i++) {
            tot += wrong[i].get();
        }
        System.exit(tot > 0 ? 3 : 0);
    }
}
