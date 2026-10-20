// Interpreter round i2 wave 58, lane L2: a thread that is ALREADY running a
// hot compiled loop when another thread takes the monitor of the synchronized
// method the loop calls blocks inside compiled code; HotSpot lists the
// synchronized callee on top of its stack trace (its frame is built before
// `lock_method`).
// docs/internal/fixed-bugs/interpreter-L6-a-blocked-synchronized-method-entrant-is-missing-from-its-stack-trace-FIXED-20261007.md
// ("Wave 57", "Wave 58").
//
// Rows: an instance callee the optimizing tier splices (`inc`, the wave-57
// shape, first and again last), a static synchronized callee (`sinc`),
// callees too large to splice (`incBig`, `sincBig`) and a callee called from
// a second level of compiled code (`outer` -> `spin`).
//
// CratonVM after wave 58 needs `CRATONVM_JIT_IR_SYNC_SPLICE_ENTER_NAMES_CALLEE`
// (on by default from the lane's switch-on commit): `=0` reproduces the old
// lines below, the positive control.
//
// Expected output (HotSpot 25.0.3, default flags, the Windows box):
//
//   inc: BLOCKED inc < spin < lambda$main$0
//   sinc: BLOCKED sinc < spinStatic < lambda$main$1
//   incBig: BLOCKED incBig < spinBig < lambda$main$2
//   sincBig: BLOCKED sincBig < spinStaticBig < lambda$main$3
//   nested: BLOCKED inc < spin < outer
//   inc again: BLOCKED inc < spin < lambda$main$4
//   counted: true
//   done
//
// CratonVM before wave 58 (debug build, default JIT): the spliced rows print
// the caller on top (`BLOCKED spin < lambda$main$0 < run`): the spliced
// enter blocks in the caller's frame, at the invoke. `--nojit` matches
// HotSpot. Racy by nature (the looper may be caught between iterations,
// where it blocks at the next enter): run it several times.
public class L2W58BlockedEntrantInCompiledCallers {
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
        StringBuilder s = new StringBuilder(trace[0].getMethodName());
        for (int i = 1; i < trace.length && i < 3; i++) {
            s.append(" < ").append(trace[i].getMethodName());
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
        looper("sinc", L2W58BlockedEntrantInCompiledCallers.class, () -> spinStatic(100_000));
        looper("incBig", c, () -> spinBig(c, 100_000));
        looper("sincBig", L2W58BlockedEntrantInCompiledCallers.class, () -> spinStaticBig(100_000));
        nested(c);
        looper("inc again", c, () -> spin(c, 100_000));
        long before = c.n;
        spin(c, 1000);
        System.out.println("counted: " + (c.n - before == 1000L));
        System.out.println("done");
    }
}
