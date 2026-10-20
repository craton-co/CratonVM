// The i54-L6 retirement: tools/probes/interp/L2/L2W58BlockedEntrantInCompiledCallers.java
// with each frame's LINE as well as its name. A thread blocked entering a
// synchronized method whose compiled body takes the monitor in its own
// prologue (`incBig`, `sincBig`: too large to splice, called directly by the
// compiled caller) listed that method at line -1: the prologue publishes the
// method-entry poll's synthetic safepoint id, which the compiled-frame walk
// refused (`jit::conservative_roots::activation_bci_with`). HotSpot lists the
// method's first line (its frame at bytecode index 0, in `lock_method`).
// Racy by nature like the original: run it several times. Expected output
// (HotSpot 25.0.3, default flags, the Windows box) at the end of the file.
public class L6K54BlockedCompiledEntrantLines {
    static final class Counter {
        long n;

        synchronized void inc() {
            n++;
        }

        synchronized void incBig(long by) {
            long v = n;
            v += by * 3 + (by >>> 2) + (by ^ 0x55);
            v ^= by << 1;
            v += (by * 7) / 3 + (by % 11) + (by & 0x3F);
            v -= (by >>> 5) * 13 + (by | 0x101);
            v += Long.rotateLeft(by, 7) ^ Long.rotateRight(by, 3);
            v ^= (v >>> 17) + (by << 9);
            v += (by * by) % 977 + (v & 0xFF);
            v -= (v >>> 29) ^ (by * 31);
            n = v;
        }
    }

    static long statics;

    static synchronized void sinc() {
        statics++;
    }

    static synchronized void sincBig(long by) {
        long v = statics;
        v += by * 3 + (by >>> 2) + (by ^ 0x55);
        v ^= by << 1;
        v += (by * 7) / 3 + (by % 11) + (by & 0x3F);
        v -= (by >>> 5) * 13 + (by | 0x101);
        v += Long.rotateLeft(by, 7) ^ Long.rotateRight(by, 3);
        v ^= (v >>> 17) + (by << 9);
        v += (by * by) % 977 + (v & 0xFF);
        v -= (v >>> 29) ^ (by * 31);
        statics = v;
    }

    static void spinStaticBig(int rounds) {
        for (int i = 0; i < rounds; i++) {
            sincBig(i);
        }
    }

    static volatile boolean stop;

    static void spin(Counter c, int rounds) {
        for (int i = 0; i < rounds; i++) {
            c.inc();
        }
    }

    static void spinStatic(int rounds) {
        for (int i = 0; i < rounds; i++) {
            sinc();
        }
    }

    static void spinBig(Counter c, int rounds) {
        for (int i = 0; i < rounds; i++) {
            c.incBig(i);
        }
    }

    static void outer(Counter c) {
        while (!stop) {
            spin(c, 100_000);
        }
    }

    static String describe(StackTraceElement[] trace) {
        if (trace.length == 0) {
            return "(empty)";
        }
        StringBuilder s = new StringBuilder(trace[0].getMethodName() + ":" + trace[0].getLineNumber());
        for (int i = 1; i < trace.length && i < 3; i++) {
            s.append(" < ").append(trace[i].getMethodName() + ":" + trace[i].getLineNumber());
        }
        return s.toString();
    }

    static void awaitBlocked(Thread t) throws InterruptedException {
        long until = System.currentTimeMillis() + 5_000;
        while (t.getState() != Thread.State.BLOCKED && System.currentTimeMillis() < until) {
            Thread.sleep(1);
        }
    }

    static void looper(String label, Object lock, Runnable body) throws InterruptedException {
        stop = false;
        Thread t = new Thread(() -> {
            while (!stop) {
                body.run();
            }
        }, "looper");
        t.setDaemon(true);
        t.start();
        Thread.sleep(300);
        synchronized (lock) {
            awaitBlocked(t);
            System.out.println(label + ": " + t.getState() + " " + describe(t.getStackTrace()));
        }
        stop = true;
        t.join();
    }

    static void nested(Counter c) throws InterruptedException {
        stop = false;
        Thread t = new Thread(() -> outer(c), "nested");
        t.setDaemon(true);
        t.start();
        Thread.sleep(300);
        synchronized (c) {
            awaitBlocked(t);
            System.out.println("nested: " + t.getState() + " " + describe(t.getStackTrace()));
        }
        stop = true;
        t.join();
    }

    public static void main(String[] args) throws Exception {
        Counter c = new Counter();
        for (int r = 0; r < 20; r++) {
            spin(c, 10_000);
            spinStatic(10_000);
            spinBig(c, 10_000);
            spinStaticBig(10_000);
        }
        looper("inc", c, () -> spin(c, 100_000));
        looper("sinc", L6K54BlockedCompiledEntrantLines.class, () -> spinStatic(100_000));
        looper("incBig", c, () -> spinBig(c, 100_000));
        looper("sincBig", L6K54BlockedCompiledEntrantLines.class, () -> spinStaticBig(100_000));
        nested(c);
        looper("inc again", c, () -> spin(c, 100_000));
        long before = c.n;
        spin(c, 1000);
        System.out.println("counted: " + (c.n - before == 1000L));
        System.out.println("done");
    }
}

// Expected output (HotSpot 25.0.3, default flags and -Xint, the Windows box;
// CratonVM before the fix: `incBig:-1`, `sincBig:-1`, and racily `inc:-1` / `sinc:-1`):
//
//   inc: BLOCKED inc:16 < spin:62 < lambda$main$0:142
//   sinc: BLOCKED sinc:36 < spinStatic:68 < lambda$main$1:143
//   incBig: BLOCKED incBig:20 < spinBig:74 < lambda$main$2:144
//   sincBig: BLOCKED sincBig:40 < spinStaticBig:54 < lambda$main$3:145
//   nested: BLOCKED inc:16 < spin:62 < outer:80
//   inc again: BLOCKED inc:16 < spin:62 < lambda$main$4:147
//   counted: true
//   done
