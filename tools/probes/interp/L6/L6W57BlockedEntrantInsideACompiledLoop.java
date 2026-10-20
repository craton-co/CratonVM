// Interpreter round i2 wave 57, lane L6: a thread blocked entering a
// synchronized method that a COMPILED loop calls, while it runs that loop
// (docs/internal/fixed-bugs/interpreter-L6-a-blocked-synchronized-method-entrant-is-missing-from-its-stack-trace-FIXED-20261007.md,
// "Wave 57").
//
// `spin` calls the synchronized `Counter.inc` in a loop and is made hot
// first. The first three reports start a fresh thread whose call of `spin`
// blocks on its first `inc` (the shapes waves 55 and 56 fixed); the
// `looper` reports take the monitor while a thread is ALREADY running the
// hot loop, so it blocks inside compiled code. HotSpot lists `inc` on top in
// every report.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   inc cold: BLOCKED inc < spin < lambda$main$0
//   incBig cold: BLOCKED incBig < spinBig < lambda$main$1
//   inc again: BLOCKED inc < spin < lambda$main$2
//   looper 0: BLOCKED inc < spin < lambda$main$3
//   looper 1: BLOCKED inc < spin < lambda$main$3
//   looper 2: BLOCKED inc < spin < lambda$main$3
//   done
//
// CratonVM (wave 57, debug build, `jdk-only` and `--compatible`) prints the
// three `looper` lines as `BLOCKED spin < lambda$main$3 < run`: the compiled
// view (`CRATONVM_DBG_SWCHAIN=1`: `activations=1 [...spin...]`) has no
// `inc`. `--nojit` matches.
public class L6W57BlockedEntrantInsideACompiledLoop {
    static final class Counter {
        long n;

        synchronized void inc() {
            n++;
        }

        synchronized void incBig(long by) {
            n += by * 3 + (by >>> 2) + (by ^ 0x55);
            n ^= by << 1;
        }
    }

    static volatile boolean stop;

    static void spin(Counter c, int rounds) {
        for (int i = 0; i < rounds; i++) {
            c.inc();
        }
    }

    static void spinBig(Counter c, int rounds) {
        for (int i = 0; i < rounds; i++) {
            c.incBig(i);
        }
    }

    static String describe(StackTraceElement[] trace) {
        if (trace.length == 0) {
            return "(empty)";
        }
        StringBuilder s = new StringBuilder(trace[0].getMethodName());
        for (int i = 1; i < trace.length && i < 3; i++) {
            s.append(" < ").append(trace[i].getMethodName());
        }
        return s.toString();
    }

    static void awaitBlocked(Thread t) throws InterruptedException {
        long until = System.currentTimeMillis() + 3_000;
        while (t.getState() != Thread.State.BLOCKED && System.currentTimeMillis() < until) {
            Thread.sleep(1);
        }
    }

    static void report(String label, Counter c, Runnable entry) throws InterruptedException {
        Thread t;
        synchronized (c) {
            t = new Thread(entry, "entrant");
            t.setDaemon(true);
            t.start();
            awaitBlocked(t);
            System.out.println(label + ": " + t.getState() + " " + describe(t.getStackTrace()));
        }
        t.join(10_000);
    }

    public static void main(String[] args) throws Exception {
        Counter c = new Counter();
        for (int r = 0; r < 20; r++) {
            spin(c, 10_000);
            spinBig(c, 10_000);
        }
        report("inc cold", c, () -> spin(c, 1));
        report("incBig cold", c, () -> spinBig(c, 1));
        report("inc again", c, () -> spin(c, 1));
        // A thread already inside the hot loop when the monitor is taken.
        Thread looper = new Thread(() -> {
            while (!stop) {
                spin(c, 1_000_000);
            }
        }, "looper");
        looper.setDaemon(true);
        looper.start();
        Thread.sleep(500);
        for (int k = 0; k < 3; k++) {
            synchronized (c) {
                awaitBlocked(looper);
                System.out.println("looper " + k + ": " + looper.getState() + " "
                        + describe(looper.getStackTrace()));
            }
            Thread.sleep(50);
        }
        stop = true;
        looper.join(10_000);
        System.out.println("done");
    }
}
