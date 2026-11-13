// j27 wave 4 (lane MONROWS): the JIT monitor / sync-door rows of
// docs/feature-designs/jit-ir-optimizations.md (MON17-5,
// MON17W2-2, MON3-4, MON5-5, SDB17-3, SDB17-4, SDB17-5, SDR17-1, SY16-3).
// Every row is a Java-visible monitor answer that must not move when the
// compiled monitor code changes: totals, which lock is held, the exception and
// its message. Each method is called hot first, so both tiers compile it.
//
//   javac -d /tmp/j27mon tools/probes/jit/j27/J27MonitorRows.java
//   java     -cp /tmp/j27mon J27MonitorRows
//   cratonvm -cp /tmp/j27mon J27MonitorRows                    (jdk-only)
//   cratonvm --compatible -cp /tmp/j27mon J27MonitorRows
//   CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_ARRAY_OPS=1 cratonvm -cp /tmp/j27mon J27MonitorRows
//   (MON17-5's opt-in arm: the same lines; with CRATONVM_DBG_JITC=1 the
//   `ring` / `nested` / `ringAt` loops log "coarsened" instead of "REFUSED ...
//   a body node could call, allocate or resume elsewhere")
//   CRATONVM_JIT_IR_LOOP_LOCK_COARSEN_DOMINATED_FIELD_BASE=1 cratonvm -cp /tmp/j27mon J27MonitorRows
//   (MON17W2-2's opt-in arm: the same lines; `holderLoop` coarsens instead of
//   "the monitor's getfield base is not the receiver", and the null-holder /
//   null-lock rows keep HotSpot's NPE)
//
// The JVMTI arm (SDB17-5): a C shim, loaded as an agent and with System.load,
// arms `Exception` + `ExceptionCatch` for this thread AFTER `tcb` / `tcm`
// were compiled and counts the events of their catches:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libj27mon.so tools/probes/jit/j27/J27MonitorRows.c
//   java     -agentpath:/tmp/libj27mon.so -cp /tmp/j27mon J27MonitorRows /tmp/libj27mon.so
//   cratonvm -agentpath:/tmp/libj27mon.so -cp /tmp/j27mon J27MonitorRows /tmp/libj27mon.so
//
// Expected HotSpot 25 output (computed off-line from the Java below and the
// JEP 358 / ObjectMonitor message rules; confirm on the HotSpot reference
// run before judging CratonVM against it):
//
//   fact ring-total 500000
//   fact ring-holds false
//   fact nested-total 2499750000
//   fact nested-holds false
//   fact ring-aioobe Index 16 out of bounds for length 16
//   fact ring-aioobe-holds false
//   fact ring-aioobe-other-thread-entered true
//   fact field-monitor-sum 400000
//   fact field-monitor-null-holder Cannot read field "lock" because "<parameter1>" is null
//   fact field-monitor-null-lock Cannot enter synchronized block because "<parameter1>.lock" is null
//   fact holder-loop 49995000
//   fact holder-loop-null-holder Cannot read field "lock" because "<parameter1>" is null
//   fact tcb-sum 270000
//   fact tcm-sum 270000
//   fact tc-holds-after false
//   fact imse-wait current thread is not owner
//   fact imse-notify current thread is not owner
//   fact imse-notifyall current thread is not owner
//   fact imse-after-block 10000
//   fact mt-block-total 800000
//   fact mt-method-total 800000
//   fact mt-instance-total 800000
//   fact mt-nested-array-total 800000
//   fact mt-holds false
//
// and, with the agent argument only:
//
//   fact jvmti-exception-events 2000
//   fact jvmti-catch-events 2000
//   fact jvmti-catch-in-tcb 1000
//   fact jvmti-catch-in-tcm 1000
//
// A CratonVM line that differs is a monitor defect: a lost or doubled
// increment (a hold not exclusive), `holds true` (a leaked hold), a missing
// `other-thread-entered` (a hold leaked across the exception), a wrong
// message, or a JVMTI count short of HotSpot's (a compiled catch that posted
// nothing).
public class J27MonitorRows {
    static final Object LOCK = new Object();
    static final Object OUTER = new Object();
    static final Object INNER = new Object();
    static final int[] RING = new int[16];
    static final int[] RING2 = new int[16];
    static final long[] NEST = new long[64];
    static final int[] SHARED = new int[8];
    static long blockCount;
    static long methodCount;

    static native void arm(Class<?> marker);
    static native void disarm();
    static native int exceptionEvents();
    static native int catchEvents();
    static native int catchesIn(String method);

    static final class Holder {
        final Object lock;

        Holder(Object lock) {
            this.lock = lock;
        }
    }

    /** The probe's own throwable, so the JVMTI shim counts nothing else. */
    static final class Tc extends RuntimeException {
        Tc(String message) {
            super(message);
        }
    }

    static final class Counter {
        long value;

        synchronized void inc() {
            value++;
        }
    }

    static void fact(String name, Object value) {
        System.out.println("fact " + name + " " + value);
    }

    static boolean holdsAny() {
        return Thread.holdsLock(LOCK)
                || Thread.holdsLock(OUTER)
                || Thread.holdsLock(INNER)
                || Thread.holdsLock(J27MonitorRows.class);
    }

    // ── MON17-5: a block whose body indexes an array ──────────────────

    static void ring(int n) {
        for (int i = 0; i < n; i++) {
            synchronized (LOCK) {
                RING[i & 15] = RING[i & 15] + 1;
            }
        }
    }

    static void nested(int n) {
        for (int i = 0; i < n; i++) {
            synchronized (OUTER) {
                synchronized (INNER) {
                    NEST[i & 63] += i;
                }
            }
        }
    }

    /** `bad >= 0`: iteration `bad` stores past the end, inside the block. */
    static int ringAt(int n, int bad) {
        int c = 0;
        for (int i = 0; i < n; i++) {
            synchronized (LOCK) {
                RING2[i == bad ? 16 : (i & 15)] = 7;
                c++;
            }
        }
        return c;
    }

    static void rows() {
        for (int r = 0; r < 50; r++) {
            ring(10_000);
        }
        long ringTotal = 0;
        for (int v : RING) {
            ringTotal += v;
        }
        fact("ring-total", ringTotal);
        fact("ring-holds", holdsAny());

        for (int r = 0; r < 50; r++) {
            nested(10_000);
        }
        long nestTotal = 0;
        for (long v : NEST) {
            nestTotal += v;
        }
        fact("nested-total", nestTotal);
        fact("nested-holds", holdsAny());

        for (int r = 0; r < 20; r++) {
            ringAt(10_000, -1);
        }
        String aioobe = "none";
        try {
            ringAt(10_000, 5_000);
        } catch (ArrayIndexOutOfBoundsException e) {
            aioobe = e.getMessage();
        }
        fact("ring-aioobe", aioobe);
        fact("ring-aioobe-holds", holdsAny());
        fact("ring-aioobe-other-thread-entered", otherThreadEnters(LOCK));
    }

    /** Can another thread take `lock` now? Bounded, so a leak cannot hang. */
    static boolean otherThreadEnters(Object lock) {
        final boolean[] entered = new boolean[1];
        Thread t = new Thread(() -> {
            synchronized (lock) {
                entered[0] = true;
            }
        });
        t.setDaemon(true);
        t.start();
        try {
            t.join(10_000);
        } catch (InterruptedException e) {
            return false;
        }
        return entered[0];
    }

    // ── MON17W2-2: a field monitor through a non-receiver base ─────────

    static int fieldMon(Holder h, int v) {
        synchronized (h.lock) {
            return v + 1;
        }
    }

    static long holderLoop(Holder h, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            synchronized (h.lock) {
                s += i;
            }
        }
        return s;
    }

    static void fieldRows() {
        Holder h = new Holder(new Object());
        long s = 0;
        for (int i = 0; i < 400_000; i++) {
            s += fieldMon(h, 0);
        }
        fact("field-monitor-sum", s);
        fact("field-monitor-null-holder", npeOf(() -> fieldMon(null, 1)));
        fact("field-monitor-null-lock", npeOf(() -> fieldMon(new Holder(null), 1)));
        long loop = 0;
        for (int r = 0; r < 50; r++) {
            loop = holderLoop(h, 10_000);
        }
        fact("holder-loop", loop);
        fact("holder-loop-null-holder", npeOf(() -> holderLoop(null, 10)));
    }

    static String npeOf(Runnable r) {
        try {
            r.run();
            return "none";
        } catch (NullPointerException e) {
            return e.getMessage();
        }
    }

    // ── SDB17-4 / SDB17-5: a catch inside a compiled synchronized body ──

    static int tcb(int i) {
        synchronized (LOCK) {
            try {
                if ((i & 7) == 0) {
                    throw new Tc("j27tc");
                }
                return 1;
            } catch (Tc e) {
                return Thread.holdsLock(LOCK) ? 2 : -1000;
            }
        }
    }

    static void maybeThrow(int i) {
        if ((i & 7) == 0) {
            throw new Tc("j27tc");
        }
    }

    static synchronized int tcm(int i) {
        try {
            maybeThrow(i);
            return 1;
        } catch (Tc e) {
            return Thread.holdsLock(J27MonitorRows.class) ? 2 : -1000;
        }
    }

    static long tcRound() {
        long s = 0;
        for (int i = 0; i < 8_000; i++) {
            s += tcb(i);
        }
        return s;
    }

    static long tcmRound() {
        long s = 0;
        for (int i = 0; i < 8_000; i++) {
            s += tcm(i);
        }
        return s;
    }

    static void catchRows(String agentLib) {
        long b = 0;
        long m = 0;
        for (int r = 0; r < 30; r++) {
            b += tcRound();
            m += tcmRound();
        }
        fact("tcb-sum", b);
        fact("tcm-sum", m);
        fact("tc-holds-after", holdsAny());
        if (agentLib == null) {
            return;
        }
        arm(Tc.class);
        tcRound();
        tcmRound();
        disarm();
        fact("jvmti-exception-events", exceptionEvents());
        fact("jvmti-catch-events", catchEvents());
        fact("jvmti-catch-in-tcb", catchesIn("tcb"));
        fact("jvmti-catch-in-tcm", catchesIn("tcm"));
    }

    // ── IllegalMonitorStateException ──────────────────────────────────

    static String imse(Object o, int mode) {
        try {
            if (mode == 0) {
                o.wait(1);
            } else if (mode == 1) {
                o.notify();
            } else {
                o.notifyAll();
            }
            return "none";
        } catch (IllegalMonitorStateException e) {
            return e.getMessage();
        } catch (InterruptedException e) {
            return "interrupted";
        }
    }

    /** A balanced block, then a notify outside it: always the IMSE. */
    static int imseAfterBlock(Object o) {
        synchronized (o) {
            blockCount += 0;
        }
        try {
            o.notify();
            return 0;
        } catch (IllegalMonitorStateException e) {
            return 1;
        }
    }

    static void imseRows() {
        Object o = new Object();
        for (int i = 0; i < 20_000; i++) {
            imse(o, 1);
        }
        fact("imse-wait", imse(o, 0));
        fact("imse-notify", imse(o, 1));
        fact("imse-notifyall", imse(o, 2));
        int n = 0;
        for (int i = 0; i < 10_000; i++) {
            n += imseAfterBlock(o);
        }
        fact("imse-after-block", n);
    }

    // ── Multithreaded totals ──────────────────────────────────────────

    static synchronized void incMethod() {
        methodCount++;
    }

    static void worker(Counter c, int n) {
        for (int i = 0; i < n; i++) {
            synchronized (LOCK) {
                blockCount++;
            }
            incMethod();
            c.inc();
            synchronized (OUTER) {
                synchronized (INNER) {
                    SHARED[i & 7]++;
                }
            }
        }
    }

    static void threadRows() throws InterruptedException {
        blockCount = 0;
        methodCount = 0;
        Counter c = new Counter();
        Thread[] ts = new Thread[4];
        for (int t = 0; t < ts.length; t++) {
            ts[t] = new Thread(() -> worker(c, 200_000));
        }
        for (Thread t : ts) {
            t.start();
        }
        for (Thread t : ts) {
            t.join();
        }
        long shared = 0;
        for (int v : SHARED) {
            shared += v;
        }
        fact("mt-block-total", blockCount);
        fact("mt-method-total", methodCount);
        fact("mt-instance-total", c.value);
        fact("mt-nested-array-total", shared);
        fact("mt-holds", holdsAny());
    }

    public static void main(String[] args) throws InterruptedException {
        String agentLib = args.length > 0 ? args[0] : null;
        if (agentLib != null) {
            System.load(agentLib);
        }
        rows();
        fieldRows();
        catchRows(agentLib);
        imseRows();
        threadRows();
    }
}
